//! The management API under `/api/v1`, used by the dashboard.
//!
//! The routes follow [`mesh_core::api`]. The caller must pass the
//! `[consumers]` policy, and the router that serves these routes adds the
//! caller's [`crate::peer::Peer`] as a request extension. Changes go to the
//! daemon that owns the subscription, and only its owner may make them. The
//! log shows each caller only their own requests.

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post, put};
use mesh_core::api::{
    ACCESS_REQUEST_PATH, LOGS_PATH, OVERVIEW_PATH, SESSION_PATH, SETTINGS_PATH, USAGE_PATH,
};

use crate::AppState;

mod command;
mod failure;
mod logs;
mod origin;
mod overview;
mod session;
mod settings;
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
        .route(SETTINGS_PATH, get(settings::get))
        .route(
            "/api/v1/settings/consumers/{list}/{value}",
            put(settings::add_consumer).delete(settings::remove_consumer),
        )
        .route(
            "/api/v1/settings/daemons/{list}/{value}",
            put(settings::add_daemon).delete(settings::remove_daemon),
        )
        .route(
            "/api/v1/settings/values/{name}",
            put(settings::set_value).delete(settings::reset_value),
        )
        .route(
            "/api/v1/settings/requests/{login}",
            delete(settings::forget),
        )
        .route(
            "/api/v1/settings/requests/{login}/approve",
            post(settings::approve),
        )
        .route(
            "/api/v1/settings/requests/{login}/deny",
            post(settings::deny),
        )
        .layer(middleware::from_fn(origin::same_origin_changes))
}

pub fn session_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(SESSION_PATH, get(session::session))
        .route(
            ACCESS_REQUEST_PATH,
            post(session::request_access).layer(DefaultBodyLimit::max(4096)),
        )
        .layer(middleware::from_fn(origin::same_origin_changes))
}

#[cfg(test)]
mod tests;
