//! Peer checks for the routes that need an identified caller.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::peer::{Decision, Grant, Peer, Role};

/// Check the peer at `addr`, which sent `headers`, for `role`, or produce
/// the `403` response.
pub async fn authorize(
    state: &AppState,
    addr: SocketAddr,
    headers: &HeaderMap,
    role: Role,
) -> Result<Peer, Box<Response>> {
    authorize_grant(state, addr, headers, role)
        .await
        .map(|(peer, _)| peer)
}

pub async fn authorize_grant(
    state: &AppState,
    addr: SocketAddr,
    headers: &HeaderMap,
    role: Role,
) -> Result<(Peer, Grant), Box<Response>> {
    match state.peers.check_grant(addr, headers, role).await {
        Ok(granted) => Ok(granted),
        Err(reason) => {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub peer: Peer,
    pub consumer: bool,
}

pub async fn identified(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = match state
        .peers
        .check(addr, request.headers(), Role::Consumer)
        .await
    {
        Decision::Allow(peer) => Caller {
            peer,
            consumer: true,
        },
        Decision::Deny(_) => match state.peers.identify(addr, request.headers()).await {
            Ok(peer) => Caller {
                peer,
                consumer: false,
            },
            Err(reason) => {
                tracing::warn!(%addr, %reason, "refused unidentified caller");
                return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
            }
        },
    };
    request.extensions_mut().insert(caller);
    next.run(request).await
}
