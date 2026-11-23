//! Relay and control routes served over the daemon-initiated tunnel.
//!
//! Over the tunnel a WebSocket is an HTTP/2 extended `CONNECT`, so the route
//! accepts any method.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bytes::Bytes;
use chrono::Utc;
use http::header::{CONTENT_ENCODING, COOKIE, HeaderName, HeaderValue, SET_COOKIE};
use http::{HeaderMap, Method, StatusCode, Uri};
use mesh_core::protocol::{
    DENIED_HEADER, DENIED_MODEL, HEADROOM_HEADER, HeadroomHint, MODEL_HEADER, RELAY_PREFIX,
};
use mesh_core::{AccountId, Admission, DenyReason, Window};
use mesh_providers::{Credentials, ProviderError};
use mesh_relay::{MeshHeaders, UpstreamSocket};
use serde::Deserialize;
use url::Url;

use crate::api;
use crate::state::{AccountState, AppState};

mod frames;

/// Largest request body the relay buffers.
const BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            &format!("{RELAY_PREFIX}/{{account_id}}/{{*path}}"),
            any(relay),
        )
        .with_state(state.clone())
        .merge(api::control_router(state))
}

async fn relay(
    State(state): State<Arc<AppState>>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let Some((raw_id, path_and_query)) = split_relay_uri(&parts.uri) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    let Some(account) = raw_id
        .parse::<AccountId>()
        .ok()
        .and_then(|id| state.account(&id))
    else {
        return unknown_account();
    };

    if let Some(response) = refusal(&state, &account) {
        return response;
    }
    let url = match state
        .settings
        .endpoints(account.provider)
        .upstream_url(&path_and_query)
    {
        Ok(url) => url,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("bad upstream path: {err}\n"),
            )
                .into_response();
        }
    };
    let call = Call {
        state: &state,
        account: &account,
        url,
        headers: &parts.headers,
    };
    if mesh_relay::is_websocket_request(&parts) {
        let Ok(upgrade) = ws else {
            return (StatusCode::BAD_REQUEST, "invalid WebSocket upgrade\n").into_response();
        };
        if let Some(model) = parts
            .headers
            .get(MODEL_HEADER)
            .and_then(|value| value.to_str().ok())
            && !state.model_allowed(&account, model)
        {
            tracing::info!(account = %account.id, %model, "WebSocket model not allowed");
            return denied_model_response(model);
        }
        let credentials = match call.credentials().await {
            Ok(credentials) => credentials,
            Err(response) => return *response,
        };
        return call.websocket(upgrade, credentials).await;
    }
    let mut stop = account.subscribe_stop();
    let body = tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => return unknown_account(),
        result = axum::body::to_bytes(body, BODY_LIMIT) => result,
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large\n").into_response();
        }
    };
    let credentials = match call.credentials().await {
        Ok(credentials) => credentials,
        Err(response) => return *response,
    };
    call.http(parts.method, body, credentials).await
}

fn upstream_failure(account: &AccountState, err: mesh_relay::Error) -> Response {
    match err {
        mesh_relay::Error::Http(err) => {
            tracing::warn!(account = %account.id, error = %err.without_url(), "upstream request failed");
        }
        _ => tracing::warn!(account = %account.id, "upstream WebSocket connection failed"),
    }
    (StatusCode::BAD_GATEWAY, "upstream connection failed\n").into_response()
}

fn unknown_account() -> Response {
    (StatusCode::NOT_FOUND, "unknown account\n").into_response()
}

fn refusal(state: &AppState, account: &Arc<AccountState>) -> Option<Response> {
    match state.current_admission(account, Utc::now()) {
        None => Some(unknown_account()),
        Some(Admission::Deny { reason }) => Some(denied_response(&reason)),
        Some(Admission::Admit { .. }) => None,
    }
}

/// One relayed request, minus its body.
struct Call<'a> {
    state: &'a Arc<AppState>,
    account: &'a Arc<AccountState>,
    url: Url,
    headers: &'a HeaderMap,
}

impl Call<'_> {
    async fn credentials(&self) -> Result<Credentials, Box<Response>> {
        if let Some(response) = refusal(self.state, self.account) {
            return Err(Box::new(response));
        }
        self.state
            .fresh_credentials(self.account, None)
            .await
            .map_err(|err| {
                Box::new(refusal(self.state, self.account).unwrap_or_else(|| refresh_failure(&err)))
            })
    }

    fn upstream_headers(&self, credentials: &Credentials) -> HeaderMap {
        let mut headers = mesh_relay::forward_request_headers(self.headers, MeshHeaders::Strip);
        headers.remove(COOKIE);
        mesh_providers::apply_auth(self.account.provider, credentials, &mut headers);
        headers
    }

    /// After an upstream 401: refresh once. `None` when no retry makes sense.
    async fn refresh_after_401(&self, rejected: &Credentials) -> Option<Credentials> {
        match self
            .state
            .fresh_credentials(self.account, Some(&rejected.access_token))
            .await
        {
            Ok(credentials) => Some(credentials),
            Err(err) => {
                tracing::warn!(account = %self.account.id, "refresh after 401 failed: {err}");
                None
            }
        }
    }

    /// Merge windows seen in upstream headers and return the new admission.
    fn learn(&self, headers: &HeaderMap, rate_limited: bool) -> Admission {
        let now = Utc::now();
        let mut windows =
            mesh_providers::parse_rate_limit_headers(self.account.provider, headers, now);
        if rate_limited {
            mark_limited(&mut windows);
        }
        if windows.is_empty() {
            return self.state.admission(self.account, now);
        }
        self.state
            .mutate(self.account, |inner| inner.merge(windows, now))
            .1
    }

    fn model_refusal(&self, method: &Method, body: &[u8]) -> Option<Response> {
        let models = self.state.effective_models(self.account)?;
        if !self
            .headers
            .get_all(CONTENT_ENCODING)
            .iter()
            .all(|value| value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Some(unreadable_model_response());
        }
        match request_model(body) {
            BodyModel::Named(model) if !models.allows(&model) => {
                Some(denied_model_response(&model))
            }
            BodyModel::Named(_) => None,
            BodyModel::Absent
                if body.is_empty() && matches!(*method, Method::GET | Method::HEAD) =>
            {
                None
            }
            _ => Some(unreadable_model_response()),
        }
    }

    async fn http(self, method: Method, body: Bytes, mut credentials: Credentials) -> Response {
        let mut retried = false;
        let response = loop {
            if let Some(response) = refusal(self.state, self.account) {
                return response;
            }
            if let Some(response) = self.model_refusal(&method, &body) {
                return response;
            }
            let headers = self.upstream_headers(&credentials);
            let response = match mesh_relay::send(
                &self.state.upstream,
                method.clone(),
                self.url.clone(),
                headers,
                body.clone(),
            )
            .await
            {
                Ok(response) => response,
                Err(err) => {
                    return upstream_failure(self.account, err);
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED
                && !retried
                && let Some(fresh) = self.refresh_after_401(&credentials).await
            {
                retried = true;
                credentials = fresh;
                continue;
            }
            break response;
        };

        let admission = self.learn(
            response.headers(),
            response.status() == StatusCode::TOO_MANY_REQUESTS,
        );
        tracing::info!(
            account = %self.account.id,
            status = response.status().as_u16(),
            "relayed request"
        );
        let mut response =
            mesh_relay::into_response(response, MeshHeaders::Strip, headroom_headers(&admission));
        response.headers_mut().remove(SET_COOKIE);
        response
    }

    async fn websocket(self, upgrade: WebSocketUpgrade, mut credentials: Credentials) -> Response {
        let url = match to_ws_url(self.url.clone()) {
            Ok(url) => url,
            Err(message) => {
                return (StatusCode::BAD_GATEWAY, format!("{message}\n")).into_response();
            }
        };
        let mut retried = false;
        let socket = loop {
            if let Some(response) = refusal(self.state, self.account) {
                return response;
            }
            let headers = self.upstream_headers(&credentials);
            let connected = tokio::time::timeout(
                Duration::from_secs(15),
                UpstreamSocket::connect(url.clone(), headers),
            )
            .await;
            let Ok(connected) = connected else {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream WebSocket timed out\n",
                )
                    .into_response();
            };
            match connected {
                Ok(socket) => break socket,
                Err(mesh_relay::Error::Rejected {
                    status,
                    headers,
                    body,
                }) => {
                    if status == StatusCode::UNAUTHORIZED
                        && !retried
                        && let Some(fresh) = self.refresh_after_401(&credentials).await
                    {
                        retried = true;
                        credentials = fresh;
                        continue;
                    }
                    let _ = self.learn(&headers, status == StatusCode::TOO_MANY_REQUESTS);
                    let mut headers =
                        mesh_relay::forward_response_headers(&headers, MeshHeaders::Strip);
                    headers.remove(SET_COOKIE);
                    return mesh_relay::Error::Rejected {
                        status,
                        headers,
                        body,
                    }
                    .into_response();
                }
                Err(err) => {
                    return upstream_failure(self.account, err);
                }
            }
        };
        let admission = self.learn(socket.response_headers(), false);
        tracing::info!(account = %self.account.id, "relayed WebSocket");
        let hooks = frames::hooks(self.state, self.account);
        socket.bridge(upgrade, headroom_headers(&admission), hooks)
    }
}

/// Split `/relay/<id>/<path>?<query>` into the id and `<path>?<query>`.
pub fn split_relay_uri(uri: &Uri) -> Option<(&str, String)> {
    let rest = uri.path().strip_prefix(RELAY_PREFIX)?.strip_prefix('/')?;
    let (id, path) = rest.split_once('/')?;
    if id.is_empty() || path.is_empty() {
        return None;
    }
    let mut path_and_query = path.to_owned();
    if let Some(query) = uri.query() {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }
    Some((id, path_and_query))
}

/// Switch an upstream URL to the WebSocket scheme.
pub fn to_ws_url(mut url: Url) -> Result<Url, String> {
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => return Err(format!("cannot use scheme {other} for a WebSocket")),
    };
    url.set_scheme(scheme)
        .map_err(|()| format!("cannot switch {url} to {scheme}"))?;
    Ok(url)
}

/// The `x-mesh-denied` value for a refusal.
pub fn deny_kind(reason: &DenyReason) -> &'static str {
    match reason {
        DenyReason::Paused => "paused",
        DenyReason::AuthError => "auth_error",
        DenyReason::Reserve { .. } => "reserve",
        DenyReason::Limited { .. } => "limited",
    }
}

fn denied_response(reason: &DenyReason) -> Response {
    let body = serde_json::json!({
        "error": format!("account refused: {}", deny_kind(reason)),
        "reason": reason,
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(DENIED_HEADER, deny_kind(reason))],
        axum::Json(body),
    )
        .into_response()
}

/// What a request body says about its model.
#[derive(Debug, PartialEq)]
pub(crate) enum BodyModel<'a> {
    /// The body is not a JSON object, or has no `model` (or a `null` one),
    /// so the allowlist does not apply.
    Absent,
    /// The top-level string `model`.
    Named(Cow<'a, str>),
    /// A JSON object whose model cannot be read for certain: a repeated or
    /// non-string `model`, or malformed JSON. An account with an allowlist
    /// refuses it, so that the allowlist cannot be bypassed.
    Unreadable,
}

/// Read the top-level `model` of a request body, borrowed when it has no
/// escapes.
pub(crate) fn request_model(body: &[u8]) -> BodyModel<'_> {
    #[derive(Deserialize)]
    struct ModelField<'a> {
        #[serde(borrow)]
        model: Option<Cow<'a, str>>,
    }
    if !body.trim_ascii_start().starts_with(b"{") {
        return BodyModel::Absent;
    }
    match serde_json::from_slice::<ModelField<'_>>(body) {
        Ok(ModelField { model: Some(model) }) => BodyModel::Named(model),
        Ok(ModelField { model: None }) => BodyModel::Absent,
        Err(_) => BodyModel::Unreadable,
    }
}

/// The refusal for a JSON body whose model cannot be read for certain, on an
/// account with an allowlist.
fn unreadable_model_response() -> Response {
    let body = serde_json::json!({
        "error": "account refused: the request body does not name one string model",
    });
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

/// The refusal for a request whose model the account does not allow. Unlike
/// [`denied_response`], it concerns only this request: the account still
/// admits other models.
pub(crate) fn denied_model_response(model: &str) -> Response {
    let body = serde_json::json!({
        "error": format!("account refused: model {model:?} is not allowed"),
        "model": model,
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(DENIED_HEADER, DENIED_MODEL)],
        axum::Json(body),
    )
        .into_response()
}

fn refresh_failure(err: &ProviderError) -> Response {
    match err {
        ProviderError::Auth(_) => denied_response(&DenyReason::AuthError),
        other => {
            tracing::warn!("token refresh failed: {other}");
            (StatusCode::BAD_GATEWAY, "token refresh failed\n").into_response()
        }
    }
}

fn headroom_headers(admission: &Admission) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&HeadroomHint::from(admission).to_string()) {
        headers.insert(HeaderName::from_static(HEADROOM_HEADER), value);
    }
    headers
}

/// After an upstream 429, flag the windows that are full. When none is full,
/// flag the fullest one, since it is the likely cause.
pub fn mark_limited(windows: &mut [Window]) {
    let mut any_full = false;
    for window in windows.iter_mut().filter(|w| w.used >= 0.999) {
        window.limited = true;
        any_full = true;
    }
    if any_full {
        return;
    }
    if let Some(fullest) = windows.iter_mut().max_by(|a, b| a.used.total_cmp(&b.used)) {
        fullest.limited = true;
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration;
