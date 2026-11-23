//! The management API under `/api/v1`, used by the dashboard.
//!
//! The routes follow [`mesh_core::api`]. The caller must pass the
//! `[consumers]` policy, and the router that serves these routes adds the
//! caller's [`crate::peer::Peer`] as a request extension. Changes go to the
//! daemon that owns the subscription, and only its owner may make them. The
//! log shows each caller only their own requests.

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{delete, get, post, put};
use mesh_core::api::{LOGS_PATH, OVERVIEW_PATH, USAGE_PATH};

use crate::AppState;

mod command;
mod failure;
mod logs;
mod origin;
mod overview;
mod subscriptions;
mod usage;

/// The management routes. They need the caller as a request extension, for
/// example from [`crate::access::consumer`].
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(OVERVIEW_PATH, get(overview::overview))
        .route(LOGS_PATH, get(logs::logs))
        .route(USAGE_PATH, get(usage::usage))
        .route(
            "/api/v1/subscriptions/{id}/reserve",
            put(subscriptions::set_reserve),
        )
        .route(
            "/api/v1/subscriptions/{id}/models",
            put(subscriptions::set_models),
        )
        .route(
            "/api/v1/subscriptions/{id}/pause",
            post(subscriptions::pause),
        )
        .route(
            "/api/v1/subscriptions/{id}/resume",
            post(subscriptions::resume),
        )
        .route("/api/v1/subscriptions/{id}", delete(subscriptions::delete))
        .layer(middleware::from_fn(origin::same_origin_changes))
}

#[cfg(test)]
mod tests;
