//! The consumer relay: pick an account, forward, and fail over.

use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod affinity;
mod http;
mod models;
mod stream;
mod tracker;
mod usage;
mod websocket;

pub(crate) use models::ModelLists;

use axum::extract::ws::{WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{TimeDelta, Utc};
use mesh_core::Provider;
use mesh_core::api::Transport;
use mesh_core::protocol::{
    DENIED_HEADER, DENIED_MODEL, HEADROOM_HEADER, HeadroomHint, RELAY_PREFIX,
};
use mesh_relay::MeshHeaders;
use serde_json::{Value, json};

use crate::AppState;
use crate::access::authorize;
use crate::db::RequestRecord;
use crate::log::Entry;
use crate::peer::{Peer, Role};
use crate::registry::{
    Caller, Candidate, DAEMON_COOLDOWN, DEFAULT_LIMITED_COOLDOWN, DENIED_COOLDOWN, InFlightGuard,
    Registry,
};
use tracker::Tracker;

/// The longest rest an upstream `retry-after` can impose.
const MAX_LIMITED_COOLDOWN: TimeDelta = TimeDelta::hours(1);

/// The most characters of a client's `user-agent` that the log keeps.
const MAX_CLIENT_CHARS: usize = 200;

/// The most characters of an error message that the log keeps.
const MAX_ERROR_CHARS: usize = 300;

pub(crate) type Upgrade = Result<WebSocketUpgrade, WebSocketUpgradeRejection>;

/// `ANY /claude/{*path}`
pub async fn claude(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    upgrade: Upgrade,
    request: Request,
) -> Response {
    relay(&state, Provider::Claude, addr, upgrade, request).await
}

/// `ANY /codex/{*path}`
pub async fn codex(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    upgrade: Upgrade,
    request: Request,
) -> Response {
    relay(&state, Provider::Codex, addr, upgrade, request).await
}

/// The path after `/<provider>`, such as `/v1/messages`.
fn upstream_path(path: &str, provider: Provider) -> &str {
    path.strip_prefix('/')
        .and_then(|rest| rest.strip_prefix(provider.as_str()))
        .unwrap_or("")
}

fn valid_relay_path(rest: &str) -> bool {
    let Some(relative) = rest.strip_prefix('/') else {
        return false;
    };
    !relative.starts_with('/')
        && !relative.contains('\\')
        && !relative
            .split('/')
            .next()
            .is_some_and(|first| first.contains(':'))
        && !relative.split('/').any(|segment| {
            let lower = segment.to_ascii_lowercase();
            lower.contains("%2f")
                || lower.contains("%5c")
                || matches!(lower.replace("%2e", ".").as_str(), "." | "..")
        })
}

/// The path and query of `rest` on the daemon's relay route for `candidate`.
fn relay_target(candidate: &Candidate, rest: &str, query: Option<&str>) -> String {
    let mut target = format!("{RELAY_PREFIX}/{}{rest}", candidate.id);
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    target
}

/// How long an upstream `429` asks us to wait, from `retry-after` seconds.
fn limited_cooldown(headers: &HeaderMap) -> TimeDelta {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map_or(DEFAULT_LIMITED_COOLDOWN, |secs| {
            TimeDelta::seconds(i64::from(secs)).min(MAX_LIMITED_COOLDOWN)
        })
}

/// `text`, or its first characters and an ellipsis when it has more than
/// `max` characters.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn headroom_hint(headers: &HeaderMap) -> Option<HeadroomHint> {
    headers.get(HEADROOM_HEADER)?.to_str().ok()?.parse().ok()
}

/// State shared by the attempts of one consumer request.
struct Run<'a> {
    state: &'a AppState,
    provider: Provider,
    peer: Peer,
    started: Instant,
    attempts: u32,
    /// The conversation, when the request names one.
    session: Option<String>,
    /// How long the request keeps the caches it uses warm.
    cache_ttl: TimeDelta,
    /// The model the request asks for, when its body says.
    model: Option<String>,
    transport: Transport,
    method: String,
    /// The path below the provider's prefix, without the query.
    path: String,
    /// The consumer's `user-agent`, clipped.
    client: Option<String>,
}

impl<'a> Run<'a> {
    fn new(
        state: &'a AppState,
        provider: Provider,
        peer: Peer,
        session: Option<String>,
        transport: Transport,
        job: &Job<'_>,
    ) -> Self {
        let client = job
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(|agent| clip(agent.trim(), MAX_CLIENT_CHARS))
            .filter(|agent| !agent.is_empty());
        Run {
            state,
            provider,
            peer,
            started: Instant::now(),
            attempts: 0,
            session,
            cache_ttl: state.config.cache_ttl(),
            model: None,
            transport,
            method: job.method.as_str().to_owned(),
            path: job.rest.trim_start_matches('/').to_owned(),
            client,
        }
    }

    /// Take what a request body says: its session, when no header named
    /// one, a longer cache lifetime, and its model.
    fn apply(&mut self, hints: affinity::BodyHints) {
        self.session = self.session.take().or(hints.session);
        self.cache_ttl = self.cache_ttl.max(hints.ttl.unwrap_or_default());
        self.model = hints.model;
    }

    fn caller(&self) -> Caller<'_> {
        Caller {
            session: self.session.as_deref(),
            user: self.peer.user(),
        }
    }

    fn candidates(&self, transport: &'static str) -> Vec<Candidate> {
        if self.session.is_none() {
            tracing::debug!(provider = %self.provider, transport, "relay request has no session key");
        }
        self.state.with_registry(|registry| {
            registry.candidates(
                self.provider,
                &self.caller(),
                self.model.as_deref(),
                Utc::now(),
            )
        })
    }

    /// Whether the request names a model that no account of the provider
    /// may serve, whatever its state.
    fn model_unserved(&self) -> bool {
        self.model.as_deref().is_some_and(|model| {
            self.state
                .with_registry(|registry| registry.model_unserved(self.provider, model))
        })
    }

    /// Answer a request for a model that no account may serve.
    async fn model_not_found(&self) -> Response {
        let response = model_not_found(self.provider, self.model.as_deref());
        self.finish(None, response, None).await
    }

    /// Revalidate `candidate` and obtain its tunnel and an in-flight guard.
    fn start(&self, candidate: &Candidate) -> Option<(mesh_relay::tunnel::Client, InFlightGuard)> {
        let guard = self
            .state
            .with_registry(|registry| registry.begin(candidate, self.provider, Utc::now()))?;
        let Some(client) = self
            .state
            .tunnel_client(&candidate.node, candidate.generation)
        else {
            self.daemon_failed(candidate, "daemon tunnel is not connected");
            return None;
        };
        Some((client, guard))
    }

    /// Revalidate each candidate and count only sends against the attempt limit.
    fn next_attempt(
        &mut self,
        candidates: &mut impl Iterator<Item = Candidate>,
    ) -> Option<(Candidate, mesh_relay::tunnel::Client, InFlightGuard)> {
        while (self.attempts as usize) < self.state.config.max_attempts {
            let candidate = candidates.next()?;
            if let Some((client, guard)) = self.start(&candidate) {
                self.attempts += 1;
                return Some((candidate, client, guard));
            }
        }
        None
    }

    fn update_candidate<T>(
        &self,
        candidate: &Candidate,
        update: impl FnOnce(&mut Registry) -> T,
    ) -> Option<T> {
        self.state.with_registry(|registry| {
            registry
                .is_current_candidate(candidate, self.provider)
                .then(|| update(registry))
        })
    }

    fn denied(&self, candidate: &Candidate) {
        tracing::info!(account = %candidate.id, "daemon refused the request");
        let until = Utc::now() + DENIED_COOLDOWN;
        self.update_candidate(candidate, |registry| {
            registry.cooldown_account(&candidate.id, until)
        });
    }

    fn upstream_limited(&self, candidate: &Candidate, headers: &HeaderMap) {
        let cooldown = limited_cooldown(headers);
        tracing::info!(account = %candidate.id, secs = cooldown.num_seconds(), "upstream rate limit");
        let until = Utc::now() + cooldown;
        self.update_candidate(candidate, |registry| {
            registry.cooldown_account(&candidate.id, until)
        });
    }

    fn daemon_failed(&self, candidate: &Candidate, error: impl Display) {
        tracing::warn!(node = %candidate.node, %error, "daemon tunnel unusable");
        let until = Utc::now() + DAEMON_COOLDOWN;
        self.update_candidate(candidate, |registry| {
            registry.cooldown_daemon(&candidate.node, until)
        });
    }

    /// Log the final attempt, remember the serving account's caches, and
    /// follow the response body to complete the log row when it ends.
    async fn finish(
        &self,
        candidate: Option<&Candidate>,
        response: Response,
        hint: Option<HeadroomHint>,
    ) -> Response {
        let Some(entry) = self.log(candidate, response.status(), hint).await else {
            return response;
        };
        let tracker = Tracker::new(entry, self.provider, &self.path, &response);
        stream::track_response(response, tracker)
    }

    /// Log the final attempt and remember the serving account's caches. The
    /// entry completes the row. It is `None` when the log writer has stopped.
    /// The row keeps the login that owns the serving daemon, as the registry
    /// had it when the request was routed.
    /// Queue the write under the registry lock so updates reach SQLite in order.
    async fn log(
        &self,
        candidate: Option<&Candidate>,
        status: StatusCode,
        hint: Option<HeadroomHint>,
    ) -> Option<Entry> {
        let served = status.is_success() || status == StatusCode::SWITCHING_PROTOCOLS;
        let record = RequestRecord {
            id: 0,
            ts: Utc::now(),
            consumer: self.peer.node.clone(),
            login: self.peer.login.clone(),
            provider: self.provider.to_string(),
            transport: self.transport,
            method: self.method.clone(),
            path: self.path.clone(),
            model: self.model.clone(),
            client: self.client.clone(),
            session: self.session.clone(),
            account_id: candidate.map(|c| c.id.to_string()),
            owner: candidate.and_then(|c| c.owner.clone()),
            node: candidate.map(|c| c.node.clone()),
            status: status.as_u16(),
            latency_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            attempts: self.attempts,
        };
        let slot = self.state.request_log.reserve().await;
        self.state.with_registry(|registry| {
            let mut warmed = Vec::new();
            if let Some(candidate) = candidate
                && registry.is_current_candidate(candidate, self.provider)
            {
                if let Some(hint) = hint {
                    registry.apply_headroom(&candidate.id, hint);
                }
                if served {
                    warmed = registry.record_warmth(
                        self.provider,
                        &self.caller(),
                        &candidate.id,
                        self.cache_ttl,
                        Utc::now(),
                    );
                }
            }
            slot.map(|slot| slot.send(record, warmed, self.started))
        })
    }

    async fn exhausted(&self) -> Response {
        let message = if self.attempts == 0 {
            "mesh: no account has headroom"
        } else {
            "mesh: no account could serve the request"
        };
        self.finish(None, unavailable(self.provider, message), None)
            .await
    }
}

async fn relay(
    state: &Arc<AppState>,
    provider: Provider,
    addr: SocketAddr,
    upgrade: Upgrade,
    request: Request,
) -> Response {
    match authorize(state, addr, Role::Consumer).await {
        Ok(peer) => relay_as(state, provider, peer, upgrade, request).await,
        Err(response) => *response,
    }
}

/// Relay a request from an already identified `peer`.
pub(crate) async fn relay_as(
    state: &Arc<AppState>,
    provider: Provider,
    peer: Peer,
    upgrade: Upgrade,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let session = affinity::from_headers(provider, &parts.headers);
    let rest = upstream_path(parts.uri.path(), provider);
    if !valid_relay_path(rest) {
        return (StatusCode::BAD_REQUEST, "invalid relay path\n").into_response();
    }
    let query = parts.uri.query();
    let job = Job {
        method: parts.method,
        headers: mesh_relay::forward_request_headers(&parts.headers, MeshHeaders::Strip),
        rest,
        query,
    };
    let mut run = match upgrade {
        Ok(upgrade) => {
            return websocket::relay(state.clone(), provider, peer, session, &job, upgrade);
        }
        Err(error) if mesh_relay::is_websocket_upgrade(&parts.headers) => {
            return error.into_response();
        }
        Err(_) => Run::new(state, provider, peer, session, Transport::Http, &job),
    };
    if models::is_list(provider, &job.method, rest) {
        return models::serve(&mut run, &job).await;
    }
    let body = match http::read_body(
        &parts.headers,
        body,
        state.config.max_body_bytes,
        Duration::from_secs(120),
    )
    .await
    {
        Ok(body) => body,
        Err((status, message)) => return (status, message).into_response(),
    };
    run.apply(affinity::http_hints(provider, &parts.headers, &body).await);
    http::relay(&mut run, &job, body).await
}

/// Whether a daemon refused only because its account does not allow the
/// requested model. The account itself still admits traffic.
fn refused_for_model(status: StatusCode, headers: &HeaderMap) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        && headers
            .get(DENIED_HEADER)
            .is_some_and(|value| value == DENIED_MODEL)
}

/// What to send to each daemon.
struct Job<'a> {
    method: axum::http::Method,
    headers: HeaderMap,
    rest: &'a str,
    query: Option<&'a str>,
}

fn error_type(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "overloaded_error",
        Provider::Codex => "mesh_unavailable",
    }
}

/// The JSON body for a refusal of `provider`'s request.
fn refusal_body(provider: Provider, message: &str) -> Value {
    let error = json!({"type": error_type(provider), "message": message});
    match provider {
        Provider::Claude => json!({"type": "error", "error": error}),
        Provider::Codex => json!({"error": error}),
    }
}

/// The `503` for a request that no account can serve right now.
fn unavailable(provider: Provider, message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        refusal_body(provider, message).to_string(),
    )
        .into_response()
}

/// The JSON body for a request for a model that no account may serve, as
/// `provider`'s own clients know it: Codex stops on `model_not_found` and
/// Claude Code reports `not_found_error` and may fall back to another model.
fn model_not_found_body(provider: Provider, model: Option<&str>) -> Value {
    let message = match model {
        Some(model) => format!("mesh: no account may serve the model {model}"),
        None => "mesh: no account may serve the requested model".to_owned(),
    };
    match provider {
        Provider::Claude => json!({
            "type": "error",
            "error": {"type": "not_found_error", "message": message},
        }),
        Provider::Codex => json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": "model_not_found",
            },
        }),
    }
}

/// The `404` for a request for a model that no account may serve.
fn model_not_found(provider: Provider, model: Option<&str>) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        model_not_found_body(provider, model).to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
