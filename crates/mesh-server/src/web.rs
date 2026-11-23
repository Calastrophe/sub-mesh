//! The dashboard: the built `mesh-web` files, served at `/`.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::services::{ServeDir, ServeFile};

use crate::AppState;

/// The first path segments that belong to the server's own routes. An
/// unknown path below one of them is a plain `404`, not the dashboard.
const RESERVED: [&str; 5] = ["api", "claude", "codex", "mesh", "healthz"];

/// The routes for the dashboard in `web_dir`, or a note at `/` when there is
/// none. Add them last: routes that exist elsewhere in the router win.
///
/// The files are static and public. The data they show comes from the
/// management API, which checks the caller.
pub fn routes(web_dir: Option<&Path>) -> Router<Arc<AppState>> {
    let Some(dir) = web_dir else {
        return Router::new().route("/", get(not_installed));
    };
    let files = ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")));
    Router::new()
        .fallback_service(files)
        .layer(middleware::from_fn(keep_reserved_paths))
}

async fn not_installed() -> &'static str {
    "The mesh dashboard is not installed. Set web_dir in the server config.\n"
}

/// Refuse paths of the server's own routes that matched no route.
async fn keep_reserved_paths(request: Request, next: Next) -> Response {
    if is_reserved(request.uri().path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

fn is_reserved(path: &str) -> bool {
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
    RESERVED.contains(&first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_paths_are_the_servers_own() {
        for path in [
            "/api/v1/nothing",
            "/api",
            "/claude/v1/x",
            "/codex",
            "/mesh/v1/status",
        ] {
            assert!(is_reserved(path), "{path}");
        }
        for path in [
            "/",
            "/subscriptions",
            "/apiary",
            "/index.html",
            "/assets/app.js",
        ] {
            assert!(!is_reserved(path), "{path}");
        }
    }
}
