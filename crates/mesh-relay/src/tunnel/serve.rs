use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use axum::{Extension, Router};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use hyper_util::service::TowerToHyperService;

use super::{KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT};
use crate::Error;

/// Serve `router` as the daemon's HTTP/2 server until the tunnel closes.
///
/// Extended `CONNECT` is enabled, so axum's `WebSocketUpgrade` accepts
/// WebSockets over the tunnel. Such a request has method `CONNECT`, so the
/// route must accept it: `any(handler)` does, `get(handler)` does not. Use
/// [`crate::is_websocket_request`] to detect it.
///
/// Requests on a tunnel have no peer address of their own, so the
/// `ConnectInfo<SocketAddr>` extractor fails unless the router supplies one;
/// see [`with_connect_info`].
#[allow(clippy::result_large_err)]
pub async fn serve<I>(io: I, router: Router) -> Result<(), Error>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .enable_connect_protocol()
        .keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .serve_connection(io, TowerToHyperService::new(router))
        .await
        .map_err(Error::from)
}

/// Give every request on `router` the fixed `ConnectInfo(peer)`, normally
/// the server's address from [`crate::tunnel::Dial::peer_addr`], so
/// handlers that extract `ConnectInfo<SocketAddr>` keep working over a
/// tunnel.
///
/// Like any `Router::layer`, it covers only the routes already added, so
/// call it on the finished router.
pub fn with_connect_info(router: Router, peer: SocketAddr) -> Router {
    router.layer(Extension(ConnectInfo(peer)))
}
