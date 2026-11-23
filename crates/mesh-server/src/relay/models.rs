//! The model lists, which the server answers itself.
//!
//! Codex fetches its catalog with `GET /codex/models` and Claude Code lists
//! models with `GET /claude/v1/models`. Relayed to one account, either would
//! show that account's models only, and none of the owners' allowlists. So the
//! server asks eligible accounts of the provider within a
//! deadline, and merges the answers:
//!
//! 1. Each account's answer is kept only for the models its allowlist allows.
//! 2. Entries merge by `slug` (Codex) or `id` (Claude), first account wins,
//!    accounts in id order. Each entry is copied as the account sent it.
//! 3. The merge gets the provider's envelope again, and for Codex an `ETag`.
//!
//! The merged answer is cached per provider and query for a while. The full
//! list of ids of each account, before its allowlist, is recorded in the
//! registry (see [`crate::registry::Registry::record_models`]) so that
//! routing and the dashboard can use it.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::http::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_MATCH, IF_MODIFIED_SINCE,
    IF_NONE_MATCH, IF_RANGE, IF_UNMODIFIED_SINCE, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use futures_util::stream;
use mesh_core::protocol::DENIED_HEADER;
use mesh_core::{ApiError, Provider, model_allowed};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{Job, Run, headroom_hint, relay_target};
use crate::registry::{Candidate, InFlightGuard};

/// How long all accounts together get to answer a request for a list.
const DEADLINE: Duration = Duration::from_millis(2500);

/// How long a merged list is kept.
const TTL: Duration = Duration::from_secs(60);

/// The most merged lists kept. A list past the limit is not kept.
const MAX_CACHED: usize = 32;
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
const MAX_PARALLEL_LISTS: usize = 4;

/// The largest answer read from one account. A Codex catalog is about
/// 900 KB.
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;

/// A provider and the query of the request, which Codex needs because the
/// catalog depends on `client_version`.
type ListKey = (Provider, Option<String>);

/// The merged model lists, kept so that clients that fetch often do not make
/// every account answer each time.
pub(crate) struct ModelLists {
    deadline: Duration,
    ttl: Duration,
    cached: Mutex<HashMap<ListKey, Cached>>,
}

impl Default for ModelLists {
    fn default() -> Self {
        ModelLists::new(DEADLINE, TTL)
    }
}

/// A merged list, with what it was made from.
#[derive(Clone)]
struct Cached {
    body: Bytes,
    etag: Option<HeaderValue>,
    /// The registry's [`models_revision`](crate::registry::Registry::models_revision)
    /// when the list was made. A list from an older revision is out of date.
    revision: u64,
    made: Instant,
}

impl Cached {
    fn response(&self) -> Response {
        let mut response = Response::new(Body::from(self.body.clone()));
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(etag) = &self.etag {
            headers.insert(ETAG, etag.clone());
        }
        response
    }
}

impl ModelLists {
    /// Lists whose accounts get `deadline` to answer and that are kept for `ttl`.
    pub(crate) fn new(deadline: Duration, ttl: Duration) -> Self {
        ModelLists {
            deadline,
            ttl,
            cached: Mutex::new(HashMap::new()),
        }
    }

    /// The kept list for `key`, unless it is old or from before `revision`.
    fn fresh(&self, key: &ListKey, revision: u64) -> Option<Cached> {
        let cached = self.cached.lock().expect("model list mutex poisoned");
        cached
            .get(key)
            .filter(|list| list.revision == revision && list.made.elapsed() < self.ttl)
            .cloned()
    }

    fn keep(&self, key: ListKey, list: Cached) {
        let mut cached = self.cached.lock().expect("model list mutex poisoned");
        cached.retain(|_, kept| kept.revision >= list.revision && kept.made.elapsed() < self.ttl);
        if cached
            .get(&key)
            .is_some_and(|kept| kept.revision > list.revision)
        {
            return;
        }
        let bytes: usize = cached
            .iter()
            .filter(|(kept, _)| **kept != key)
            .map(|(_, list)| list.body.len())
            .sum();
        if list.body.len() <= MAX_CATALOG_BYTES - bytes
            && (cached.len() < MAX_CACHED || cached.contains_key(&key))
        {
            cached.insert(key, list);
        }
    }
}

/// Whether the request is for the model list of `provider`, whatever its query.
pub(super) fn is_list(provider: Provider, method: &Method, rest: &str) -> bool {
    let path = match provider {
        Provider::Codex => "/models",
        Provider::Claude => "/v1/models",
    };
    *method == Method::GET && rest == path
}

/// One account's answer.
struct Answer {
    candidate: Candidate,
    body: Bytes,
}

/// Answer a request for the model list of `run`'s provider.
///
/// A kept list answers at once. Otherwise eligible accounts are asked,
/// and the answers that arrive in time are merged. With no usable answer the
/// result is a `503`.
pub(super) async fn serve(run: &mut Run<'_>, job: &Job<'_>) -> Response {
    let state = run.state;
    let key: ListKey = (run.provider, job.query.map(str::to_owned));
    let revision = state.with_registry(|registry| registry.models_revision());
    if let Some(list) = state.model_lists.fresh(&key, revision) {
        return run.finish(None, list.response(), None).await;
    }
    let answers = gather(run, job).await;
    if answers.is_empty() {
        let message = if run.attempts == 0 {
            "mesh: no account has headroom"
        } else {
            "mesh: no account answered the model list"
        };
        return run.finish(None, failed(message), None).await;
    }
    let Some(list) = merge(run, &answers, revision) else {
        return run
            .finish(
                None,
                failed("mesh: no account sent a usable model list"),
                None,
            )
            .await;
    };
    let response = list.response();
    state.model_lists.keep(key, list);
    run.finish(None, response, None).await
}

fn failed(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: message.to_owned(),
        }),
    )
        .into_response()
}

/// The headers to ask an account with: the caller's, but asking for the
/// whole list and for text that can be read as it is.
fn list_headers(headers: &HeaderMap) -> HeaderMap {
    let mut headers = headers.clone();
    for name in [
        IF_MATCH,
        IF_NONE_MATCH,
        IF_MODIFIED_SINCE,
        IF_UNMODIFIED_SINCE,
        IF_RANGE,
        RANGE,
    ] {
        headers.remove(name);
    }
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    headers
}

/// Ask eligible accounts with bounded concurrency. Return answers received
/// before the deadline in account id order.
async fn gather(run: &mut Run<'_>, job: &Job<'_>) -> Vec<Answer> {
    let provider = run.provider;
    let eligible = run
        .state
        .with_registry(|registry| registry.eligible(provider, Utc::now()));
    let headers = list_headers(&job.headers);
    let attempts = AtomicU32::new(0);
    let shared = &*run;
    let mut asks = stream::iter(eligible)
        .map(|candidate| {
            let headers = &headers;
            let attempts = &attempts;
            async move {
                let (client, guard) = shared.start(&candidate)?;
                attempts.fetch_add(1, Ordering::Relaxed);
                ask(shared, job, headers, candidate, client, guard).await
            }
        })
        .buffer_unordered(MAX_PARALLEL_LISTS);
    let deadline = tokio::time::Instant::now() + run.state.model_lists.deadline;
    let mut answers = Vec::new();
    let mut remaining = MAX_CATALOG_BYTES;
    loop {
        match tokio::time::timeout_at(deadline, asks.next()).await {
            Ok(Some(Some(answer))) if answer.body.len() <= remaining => {
                remaining -= answer.body.len();
                answers.push(answer);
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(%provider, "model list deadline reached");
                break;
            }
        }
    }
    drop(asks);
    run.attempts += attempts.into_inner();
    answers.sort_unstable_by(|a, b| a.candidate.id.cmp(&b.candidate.id));
    answers
}

/// Ask one account for its list. A refusal rests the account as it does for
/// any relayed request. `None` says that the account has no list to give.
async fn ask(
    run: &Run<'_>,
    job: &Job<'_>,
    headers: &HeaderMap,
    candidate: Candidate,
    client: mesh_relay::tunnel::Client,
    guard: InFlightGuard,
) -> Option<Answer> {
    let _guard = guard;
    let target = relay_target(&candidate, job.rest, job.query);
    let response = match client
        .send(Method::GET, &target, headers.clone(), Bytes::new())
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if error.never_dispatched() {
                run.daemon_failed(&candidate, &error);
            } else {
                tracing::warn!(node = %candidate.node, %error, "model list request failed");
            }
            return None;
        }
    };
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        if response.headers().contains_key(DENIED_HEADER) {
            run.denied(&candidate);
        } else {
            run.upstream_limited(&candidate, response.headers());
        }
        return None;
    }
    if !status.is_success() {
        tracing::warn!(account = %candidate.id, %status, "account refused to list its models");
        return None;
    }
    let plain = response
        .headers()
        .get(CONTENT_ENCODING)
        .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity"));
    if !plain {
        tracing::warn!(account = %candidate.id, "account sent its model list encoded");
        return None;
    }
    if let Some(hint) = headroom_hint(response.headers()) {
        run.update_candidate(&candidate, |registry| {
            registry.apply_headroom(&candidate.id, hint);
        });
    }
    match axum::body::to_bytes(response.into_body(), MAX_LIST_BYTES).await {
        Ok(body) => Some(Answer { candidate, body }),
        Err(error) => {
            tracing::warn!(account = %candidate.id, %error, "cannot read the model list");
            None
        }
    }
}

/// The entries of one account's answer, each with its `slug` or `id`. Entries
/// without one are left out, since neither client can use them.
fn entries(provider: Provider, body: &[u8]) -> Option<Vec<(String, &RawValue)>> {
    #[derive(Deserialize)]
    struct Catalog<'a> {
        #[serde(borrow)]
        models: Vec<&'a RawValue>,
    }

    #[derive(Deserialize)]
    struct Page<'a> {
        #[serde(borrow)]
        data: Vec<&'a RawValue>,
    }

    let raws = match provider {
        Provider::Codex => serde_json::from_slice::<Catalog>(body).ok()?.models,
        Provider::Claude => serde_json::from_slice::<Page>(body).ok()?.data,
    };
    Some(
        raws.into_iter()
            .filter_map(|raw| Some((entry_key(provider, raw)?, raw)))
            .collect(),
    )
}

/// The model id of an entry: a Codex `slug` or a Claude `id`.
fn entry_key(provider: Provider, entry: &RawValue) -> Option<String> {
    #[derive(Deserialize)]
    struct Slug {
        slug: String,
    }

    #[derive(Deserialize)]
    struct Id {
        id: String,
    }

    let key = match provider {
        Provider::Codex => serde_json::from_str::<Slug>(entry.get()).ok()?.slug,
        Provider::Claude => serde_json::from_str::<Id>(entry.get()).ok()?.id,
    };
    (!key.is_empty()).then_some(key)
}

/// Merge the answers, which are in account id order, into one list, and
/// record each account's model ids. `None` when no answer can be read.
fn merge(run: &Run<'_>, answers: &[Answer], revision: u64) -> Option<Cached> {
    let provider = run.provider;
    let mut listings = Vec::with_capacity(answers.len());
    for answer in answers {
        match entries(provider, &answer.body) {
            Some(entries) => listings.push((&answer.candidate, entries)),
            None => {
                tracing::warn!(account = %answer.candidate.id, "account sent an unreadable model list");
            }
        }
    }
    if listings.is_empty() {
        return None;
    }
    let allowlists = run.state.with_registry(|registry| {
        let mut allowlists = Vec::with_capacity(listings.len());
        for (candidate, entries) in &listings {
            let current = registry.is_current_candidate(candidate, provider);
            if current {
                let ids = entries.iter().map(|(id, _)| id.clone()).collect();
                registry.record_models(&candidate.id, ids);
            }
            allowlists.push(current.then(|| registry.model_allowlist(&candidate.id)));
        }
        allowlists
    });
    if allowlists.iter().all(Option::is_none) {
        return None;
    }
    let mut seen = HashSet::new();
    let mut kept: Vec<(&str, &RawValue)> = Vec::new();
    for ((_, entries), allowlist) in listings.iter().zip(&allowlists) {
        let Some(allowlist) = allowlist else {
            continue;
        };
        for (id, entry) in entries {
            if model_allowed(allowlist.as_ref(), id) && seen.insert(id.as_str()) {
                kept.push((id, entry));
            }
        }
    }
    let body = render(provider, &kept);
    let etag = (provider == Provider::Codex).then(|| etag(&body));
    Some(Cached {
        body: body.into(),
        etag,
        revision,
        made: Instant::now(),
    })
}

/// The provider's envelope around `kept`, whose entries are copied as sent.
fn render(provider: Provider, kept: &[(&str, &RawValue)]) -> Vec<u8> {
    let size: usize = kept.iter().map(|(_, entry)| entry.get().len() + 1).sum();
    let mut body = Vec::with_capacity(size + 96);
    body.extend_from_slice(match provider {
        Provider::Codex => br#"{"models":["#,
        Provider::Claude => br#"{"data":["#,
    });
    for (position, (_, entry)) in kept.iter().enumerate() {
        if position > 0 {
            body.push(b',');
        }
        body.extend_from_slice(entry.get().as_bytes());
    }
    body.push(b']');
    if provider == Provider::Claude {
        let id = |entry: Option<&(&str, &RawValue)>| {
            serde_json::to_string(&entry.map(|(id, _)| *id)).unwrap_or_else(|_| "null".to_owned())
        };
        body.extend_from_slice(br#","has_more":false,"first_id":"#);
        body.extend_from_slice(id(kept.first()).as_bytes());
        body.extend_from_slice(br#","last_id":"#);
        body.extend_from_slice(id(kept.last()).as_bytes());
    }
    body.push(b'}');
    body
}

/// A quoted tag that changes with the content of `body`.
fn etag(body: &[u8]) -> HeaderValue {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    HeaderValue::from_str(&format!("\"{:016x}\"", hasher.finish()))
        .expect("hex digits make a header value")
}

#[cfg(test)]
mod tests;
