use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::{Extension, Json};
use mesh_core::api::{Direction, LogEntry};
use serde::Deserialize;

use super::failure::ApiFailure;
use crate::AppState;
use crate::db::LogOwner;
use crate::peer::Peer;

/// How many entries a call returns without `limit`.
const DEFAULT_LIMIT: u32 = 100;
/// The most entries a call may ask for.
const MAX_LIMIT: u32 = 1000;

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    limit: Option<u32>,
    before: Option<i64>,
    direction: Option<Direction>,
}

/// Whose log `peer` may read: its own. A caller with a login reads the
/// requests of that login and the requests that others made with the
/// subscriptions of that login. A caller without one, which only happens
/// without whois, reads the requests with no login that came from its node.
fn owner(peer: Peer) -> LogOwner {
    match peer.login {
        Some(login) => LogOwner::Login(login),
        None => LogOwner::Node(peer.node),
    }
}

/// `GET LOGS_PATH?limit=N&before=ID&direction=D`
pub async fn logs(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    query: Result<Query<LogsQuery>, QueryRejection>,
) -> Result<Json<Vec<LogEntry>>, ApiFailure> {
    let Query(query) = query.map_err(|error| ApiFailure::bad_request(error.body_text()))?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let owner = owner(peer);
    state
        .db
        .run(move |db| db.recent_requests(&owner, query.direction, limit, query.before))
        .await
        .map(Json)
        .map_err(|error| {
            tracing::warn!(%error, "cannot read the request log");
            ApiFailure::internal("cannot read the request log")
        })
}
