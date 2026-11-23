use std::fmt;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::header::SEC_WEBSOCKET_VERSION;
use http::{HeaderMap, HeaderValue, Method, Request, Uri};
use http_body_util::Full;
use hyper::client::conn::http2::SendRequest;
use hyper::ext::Protocol;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::{KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT, TUNNEL_AUTHORITY};
use crate::Error;
use crate::headers::{self, MeshHeaders};
use crate::websocket::{self, UpstreamSocket};

/// The server's HTTP/2 client for requests to the daemon. Clones share the
/// same multiplexed connection. Callers provide any per-request timeout.
#[derive(Clone)]
pub struct Client {
    sender: SendRequest<Full<Bytes>>,
    closed: watch::Receiver<bool>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Start HTTP/2 as the client on an upgraded tunnel connection.
    ///
    /// The connection driver runs in a spawned task until the tunnel
    /// closes. Keepalive pings use [`KEEPALIVE_INTERVAL`] and
    /// [`KEEPALIVE_TIMEOUT`].
    #[allow(clippy::result_large_err)]
    pub async fn handshake<I>(io: I) -> Result<Client, Error>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .handshake(io)
            .await?;
        let (closed_tx, closed) = watch::channel(false);
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                tracing::debug!("tunnel connection ended: {err}");
            }
            let _ = closed_tx.send(true);
        });
        Ok(Client { sender, closed })
    }

    /// Wait for the upgrade from [`crate::tunnel::accept`], then
    /// [`Client::handshake`] on it.
    #[allow(clippy::result_large_err)]
    pub async fn from_upgrade(upgrade: OnUpgrade) -> Result<Client, Error> {
        Client::handshake(upgrade.await?).await
    }

    /// True once the tunnel has closed. Requests then fail at once.
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow() || self.sender.is_closed()
    }

    /// Resolves when the tunnel closes. It does not keep the tunnel open.
    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut closed = self.closed.clone();
        async move {
            let _ = closed.wait_for(|closed| *closed).await;
        }
    }

    /// Send one request with a buffered body through the tunnel and return
    /// the response with its body still streaming.
    ///
    /// `path_and_query` starts with `/`. Request headers are filtered with
    /// [`MeshHeaders::Keep`]. All HTTP statuses are returned as responses.
    #[allow(clippy::result_large_err)]
    pub async fn send(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response, Error> {
        let mut request = Request::new(Full::new(body));
        *request.method_mut() = method;
        *request.uri_mut() = tunnel_uri(path_and_query)?;
        *request.headers_mut() = headers::forward_request_headers(&headers, MeshHeaders::Keep);
        let response = self.sender.clone().send_request(request).await?;
        Ok(response.map(Body::new))
    }

    /// Open a WebSocket to `path_and_query` on the daemon with an HTTP/2
    /// extended `CONNECT` (`:protocol websocket`).
    ///
    /// `headers` are prepared like [`UpstreamSocket::connect`] does it:
    /// generated handshake headers are dropped, and `sec-websocket-protocol`
    /// offers are kept. A non-2xx answer is [`Error::Rejected`] with
    /// the daemon's status, headers, and body, the same as a refused
    /// HTTP/1.1 handshake.
    #[allow(clippy::result_large_err)]
    pub async fn connect_websocket(
        &self,
        path_and_query: &str,
        headers: HeaderMap,
    ) -> Result<UpstreamSocket, Error> {
        let mut request = Request::new(Full::new(Bytes::new()));
        *request.method_mut() = Method::CONNECT;
        *request.uri_mut() = tunnel_uri(path_and_query)?;
        *request.headers_mut() = websocket::handshake_headers(&headers)?;
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        request
            .extensions_mut()
            .insert(Protocol::from_static("websocket"));

        let mut response = self.sender.clone().send_request(request).await?;
        if !response.status().is_success() {
            return Err(super::rejected(response).await);
        }
        let upgrade = hyper::upgrade::on(&mut response);
        let (parts, _) = response.into_parts();
        let stream =
            WebSocketStream::from_raw_socket(TokioIo::new(upgrade.await?), Role::Client, None)
                .await;
        Ok(UpstreamSocket::new(stream, parts.headers))
    }
}

#[allow(clippy::result_large_err)]
fn tunnel_uri(path_and_query: &str) -> Result<Uri, Error> {
    if !path_and_query.starts_with('/') {
        return Err(Error::Other(format!(
            "tunnel path must start with '/': {path_and_query:?}"
        )));
    }
    Uri::builder()
        .scheme("http")
        .authority(TUNNEL_AUTHORITY)
        .path_and_query(path_and_query)
        .build()
        .map_err(|err| Error::Other(format!("bad tunnel path {path_and_query:?}: {err}")))
}
