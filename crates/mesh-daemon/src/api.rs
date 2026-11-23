//! Loopback admin and tunnel control APIs (see `mesh_core::admin` and
//! [`mesh_core::protocol::CONTROL_PREFIX`]).

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use chrono::Utc;
use http::header::{AUTHORIZATION, HOST, ORIGIN};
use http::uri::Authority;
use http::{HeaderMap, StatusCode};
use mesh_core::admin::{
    AdminStatus, MODELS_PATH, RELOAD_PATH, RESERVE_PATH, STATUS_PATH, SetModels as AdminSetModels,
    SetReserve as AdminSetReserve,
};
use mesh_core::api::{SetModels, SetReserve};
use mesh_core::protocol::{CONTROL_PREFIX, ON_BEHALF_HEADER};
use mesh_core::{AccountReport, ApiError, ParseAccountIdError};
use subtle::ConstantTimeEq;

use crate::state::{AppState, StateError};

/// The admin API. Every route needs `Authorization: Bearer <token>` with the
/// given token (see `mesh_core::admin`).
pub fn admin_router(state: Arc<AppState>, token: String) -> Router {
    Router::new()
        .route(STATUS_PATH, get(status))
        .route(RELOAD_PATH, post(reload))
        .route(RESERVE_PATH, put(admin_set_reserve))
        .route(MODELS_PATH, put(admin_set_models))
        .route("/admin/v1/accounts/{id}/pause", post(admin_pause))
        .route("/admin/v1/accounts/{id}/resume", post(admin_resume))
        .route("/admin/v1/accounts/{id}", delete(admin_delete))
        .layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            admin_bearer,
        ))
        .layer(middleware::from_fn(admin_local_request))
        .with_state(state)
}

async fn admin_bearer(
    State(token): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    let mut values = request.headers().get_all(AUTHORIZATION).iter();
    let presented = values
        .next()
        .filter(|_| values.next().is_none())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented.is_some_and(|presented| presented.as_bytes().ct_eq(token.as_bytes()).into()) {
        Ok(next.run(request).await)
    } else {
        Err(ApiFailure(
            StatusCode::UNAUTHORIZED,
            "missing or wrong admin token".to_owned(),
        ))
    }
}

async fn admin_local_request(request: Request, next: Next) -> Result<Response, ApiFailure> {
    if admin_headers_allowed(request.headers()) {
        Ok(next.run(request).await)
    } else {
        Err(ApiFailure(
            StatusCode::FORBIDDEN,
            "admin requires a local HTTP client".to_owned(),
        ))
    }
}

fn admin_headers_allowed(headers: &HeaderMap) -> bool {
    if headers.contains_key(ORIGIN) || headers.contains_key("sec-fetch-site") {
        return false;
    }
    let hosts = headers.get_all(HOST);
    let mut hosts = hosts.iter();
    let host = hosts
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok());
    hosts.next().is_none()
        && host.is_some_and(|authority| {
            let host = authority.host();
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

type AdminReply = Result<Json<AdminStatus>, ApiFailure>;

fn current(state: &AppState) -> Json<AdminStatus> {
    Json(state.admin_status(Utc::now()))
}

async fn status(State(state): State<Arc<AppState>>) -> Json<AdminStatus> {
    current(&state)
}

async fn reload(State(state): State<Arc<AppState>>) -> AdminReply {
    state
        .reload()
        .await
        .map_err(|err| ApiFailure(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(current(&state))
}

async fn admin_set_reserve(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AdminSetReserve>,
) -> AdminReply {
    if let Some(id) = body.account {
        state.set_account_reserve(&id, body.reserve)?;
    } else {
        state.set_default_reserve(body.reserve)?;
    }
    Ok(current(&state))
}

async fn admin_set_models(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AdminSetModels>,
) -> AdminReply {
    state.set_account_models(&body.account, body.models)?;
    Ok(current(&state))
}

async fn admin_pause(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> AdminReply {
    set_paused(&state, &id, true)?;
    Ok(current(&state))
}

async fn admin_resume(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> AdminReply {
    set_paused(&state, &id, false)?;
    Ok(current(&state))
}

async fn admin_delete(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> AdminReply {
    let id = id.parse()?;
    state.delete_account(&id).await?;
    Ok(current(&state))
}

pub fn control_router(state: Arc<AppState>) -> Router {
    let accounts = Router::new()
        .route("/accounts/{id}/reserve", put(set_reserve))
        .route("/accounts/{id}/models", put(set_models))
        .route("/accounts/{id}/pause", post(pause))
        .route("/accounts/{id}/resume", post(resume))
        .route("/accounts/{id}", delete(remove))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .nest(CONTROL_PREFIX, accounts)
        .with_state(state)
}

type ControlReply = Result<Json<AccountReport>, ApiFailure>;

async fn authorize(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    let forbidden = |message: &str| ApiFailure(StatusCode::FORBIDDEN, message.to_owned());
    let values = request.headers().get_all(ON_BEHALF_HEADER);
    let mut logins = values.iter();
    let login = logins
        .next()
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .ok_or_else(|| forbidden("missing on-behalf-of login"))?;
    if logins.next().is_some() {
        return Err(forbidden("multiple on-behalf-of logins"));
    }

    let owner = state.refresh_owner().await;
    if !state.allows_control(owner.as_deref(), login) {
        tracing::warn!(%login, "control call rejected: not the owner");
        return Err(forbidden("not the owner of this machine"));
    }

    Ok(next.run(request).await)
}

async fn set_reserve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ControlReply {
    let id = id.parse()?;
    let body: SetReserve = serde_json::from_slice(&body)
        .map_err(|err| ApiFailure(StatusCode::BAD_REQUEST, format!("invalid body: {err}")))?;
    Ok(Json(state.set_account_reserve(&id, body.reserve)?))
}

async fn set_models(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ControlReply {
    let id = id.parse()?;
    let body: SetModels = serde_json::from_slice(&body)
        .map_err(|err| ApiFailure(StatusCode::BAD_REQUEST, format!("invalid body: {err}")))?;
    Ok(Json(state.set_account_models(&id, body.models)?))
}

async fn pause(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ControlReply {
    set_paused(&state, &id, true).map(Json)
}

async fn resume(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ControlReply {
    set_paused(&state, &id, false).map(Json)
}

async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    state.delete_account(&id.parse()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Both daemon APIs use the same JSON error shape.
#[derive(Debug)]
pub struct ApiFailure(pub StatusCode, pub String);

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<StateError> for ApiFailure {
    fn from(err: StateError) -> Self {
        Self(err.status(), err.to_string())
    }
}

impl From<ParseAccountIdError> for ApiFailure {
    fn from(err: ParseAccountIdError) -> Self {
        Self(StatusCode::BAD_REQUEST, err.to_string())
    }
}

fn set_paused(state: &AppState, id: &str, paused: bool) -> Result<AccountReport, ApiFailure> {
    Ok(state.set_paused(&id.parse()?, paused)?)
}

#[cfg(test)]
mod tests;
