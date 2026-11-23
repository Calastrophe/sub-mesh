use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::{Extension, Json};
use chrono::{TimeDelta, Utc};
use mesh_core::api::{DEFAULT_USAGE_HOURS, MAX_USAGE_HOURS, UsageReport};
use serde::Deserialize;

use super::failure::ApiFailure;
use crate::AppState;
use crate::peer::Peer;

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    hours: Option<u32>,
}

/// `GET USAGE_PATH?hours=N`
///
/// Reports what the mesh used of the subscriptions that the caller's login
/// owns. A caller without a login owns none, so its report is empty.
pub async fn usage(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    query: Result<Query<UsageQuery>, QueryRejection>,
) -> Result<Json<UsageReport>, ApiFailure> {
    let Query(query) = query.map_err(|error| ApiFailure::bad_request(error.body_text()))?;
    let hours = query.hours.unwrap_or(DEFAULT_USAGE_HOURS);
    if !(1..=MAX_USAGE_HOURS).contains(&hours) {
        return Err(ApiFailure::bad_request(format!(
            "hours must be from 1 to {MAX_USAGE_HOURS}"
        )));
    }
    let since = Utc::now() - TimeDelta::hours(i64::from(hours));
    let rows = match peer.login {
        Some(login) => state
            .db
            .run(move |db| db.usage(&login, since))
            .await
            .map_err(|error| {
                tracing::warn!(%error, "cannot read the usage");
                ApiFailure::internal("cannot read the usage")
            })?,
        None => Vec::new(),
    };
    Ok(Json(UsageReport { since, rows }))
}
