//! Peer checks for the routes that need an identified caller.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::peer::{Decision, Peer, Role};

/// Check the peer at `addr`, which sent `headers`, for `role`, or produce
/// the `403` response.
pub async fn authorize(
    state: &AppState,
    addr: SocketAddr,
    headers: &HeaderMap,
    role: Role,
) -> Result<Peer, Box<Response>> {
    match state.peers.check(addr, headers, role).await {
        Decision::Allow(peer) => Ok(peer),
        Decision::Deny(reason) => {
            tracing::warn!(%addr, ?role, %reason, "refused peer");
            Err(Box::new(
                (StatusCode::FORBIDDEN, "forbidden\n").into_response(),
            ))
        }
    }
}

/// Middleware for routes that follow the `[consumers]` policy. It refuses
/// other peers and gives the handler the caller as a [`Peer`] extension.
pub async fn consumer(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    match authorize(&state, addr, request.headers(), Role::Consumer).await {
        Ok(peer) => {
            request.extensions_mut().insert(peer);
            next.run(request).await
        }
        Err(response) => *response,
    }
}
