use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use mesh_core::api::{MAX_ACCESS_NOTE, RequestAccess, Session};

use super::failure::ApiFailure;
use crate::AppState;
use crate::access::Caller;
use crate::db::RequestOutcome;

fn database(error: anyhow::Error) -> ApiFailure {
    tracing::warn!(%error, "access request storage failed");
    ApiFailure::internal("cannot read access requests")
}

async fn build(state: &Arc<AppState>, caller: Caller) -> Result<Session, ApiFailure> {
    let admin = caller.consumer && state.peers.is_admin(&caller.peer);
    let login = caller.peer.login.clone();
    let consumer = caller.consumer;
    let (request, pending_requests) = state
        .db
        .run(move |db| {
            let request = match (&login, consumer) {
                (Some(login), false) => db.access_request(login)?,
                _ => None,
            };
            let pending = if admin {
                Some(db.pending_requests()?)
            } else {
                None
            };
            Ok((request, pending))
        })
        .await
        .map_err(database)?;
    Ok(Session {
        login: caller.peer.login,
        node: caller.peer.node,
        consumer: caller.consumer,
        admin,
        request,
        pending_requests,
    })
}

pub async fn session(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Session>, ApiFailure> {
    build(&state, caller).await.map(Json)
}

fn note(note: Option<String>) -> Result<Option<String>, ApiFailure> {
    let Some(note) = note else {
        return Ok(None);
    };
    let note = note.trim();
    if note.chars().count() > MAX_ACCESS_NOTE {
        return Err(ApiFailure::bad_request(format!(
            "the note is longer than {MAX_ACCESS_NOTE} characters"
        )));
    }
    if note.chars().any(char::is_control) {
        return Err(ApiFailure::bad_request(
            "the note cannot contain control characters",
        ));
    }
    Ok((!note.is_empty()).then(|| note.to_owned()))
}

pub async fn request_access(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Result<Json<Session>, ApiFailure> {
    if caller.consumer {
        return Err(ApiFailure::new(
            StatusCode::CONFLICT,
            "you already have access",
        ));
    }
    let Some(login) = caller.peer.login.clone() else {
        return Err(ApiFailure::forbidden("access needs a tailnet login"));
    };
    let body: RequestAccess = if body.is_empty() {
        RequestAccess::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|error| ApiFailure::bad_request(format!("invalid request: {error}")))?
    };
    let note = note(body.note)?;
    let requester = login.clone();
    let outcome = state
        .db
        .run(move |db| db.request_access(&requester, note.as_deref(), Utc::now()))
        .await
        .map_err(database)?;
    match outcome {
        RequestOutcome::Created(_) => tracing::info!(%login, "access requested"),
        RequestOutcome::Existing(_) => {}
        RequestOutcome::Full => {
            return Err(ApiFailure::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many access requests are waiting",
            ));
        }
    }
    build(&state, caller).await.map(Json)
}
