use std::net::SocketAddr;

use bytes::Bytes;
use http::header::{CONNECTION, HOST, UPGRADE};
use http::{HeaderValue, Request, StatusCode};
use http_body_util::Empty;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use mesh_core::protocol::{NODE_HEADER, TUNNEL_PATH, TUNNEL_PROTOCOL};
use tokio::net::TcpStream;
use url::Url;

use super::DIAL_TIMEOUT;
use crate::Error;
use crate::headers;

/// A TCP connection to the server, made before the tunnel upgrade so the
/// daemon can check the server's identity (for example with tailnet whois
/// on [`Dial::peer_addr`]) before it sends anything.
#[derive(Debug)]
pub struct Dial {
    stream: TcpStream,
    peer: SocketAddr,
    host: String,
}

impl Dial {
    /// Open a TCP connection to `server_url`, within [`DIAL_TIMEOUT`].
    ///
    /// Only `http://` URLs work: the tunnel runs in plain text and relies on
    /// the tailnet for encryption. The URL's path is ignored; the upgrade
    /// always goes to [`TUNNEL_PATH`].
    #[allow(clippy::result_large_err)]
    pub async fn connect(server_url: &Url) -> Result<Dial, Error> {
        if server_url.scheme() != "http" {
            return Err(Error::Other(format!(
                "tunnel needs an http:// server URL, not {server_url}"
            )));
        }
        let (Some(host), Some(port)) = (server_url.host_str(), server_url.port_or_known_default())
        else {
            return Err(Error::Other(format!(
                "server URL has no host: {server_url}"
            )));
        };
        let address = format!("{host}:{port}");
        let stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(&address))
            .await
            .map_err(|_| timed_out("connecting to the server"))??;
        let peer = stream.peer_addr()?;
        let host = match server_url.port() {
            Some(_) => address,
            None => host.to_owned(),
        };
        Ok(Dial { stream, peer, host })
    }

    /// The server's address on this connection.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Send the tunnel upgrade for `node` and return the upgraded
    /// connection, within [`DIAL_TIMEOUT`]. Pass it to
    /// [`crate::tunnel::serve`].
    ///
    /// Any answer other than `101` with `Upgrade: `[`TUNNEL_PROTOCOL`] is an
    /// error. A refusal (such as `403` from a failed identity check) is
    /// [`Error::Rejected`] with the server's status, headers, and body.
    #[allow(clippy::result_large_err)]
    pub async fn upgrade(self, node: &str) -> Result<Upgraded, Error> {
        tokio::time::timeout(DIAL_TIMEOUT, self.upgrade_now(node))
            .await
            .map_err(|_| timed_out("waiting for the tunnel upgrade"))?
    }

    #[allow(clippy::result_large_err)]
    async fn upgrade_now(self, node: &str) -> Result<Upgraded, Error> {
        let node = HeaderValue::from_str(node)
            .map_err(|err| Error::Other(format!("bad node name {node:?}: {err}")))?;
        let host = HeaderValue::from_str(&self.host)
            .map_err(|err| Error::Other(format!("bad server host: {err}")))?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(self.stream)).await?;
        tokio::spawn(async move {
            if let Err(err) = connection.with_upgrades().await {
                tracing::debug!("tunnel upgrade connection failed: {err}");
            }
        });

        let mut request = Request::new(Empty::<Bytes>::new());
        *request.uri_mut() = http::Uri::from_static(TUNNEL_PATH);
        let request_headers = request.headers_mut();
        request_headers.insert(HOST, host);
        request_headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        request_headers.insert(UPGRADE, HeaderValue::from_static(TUNNEL_PROTOCOL));
        request_headers.insert(NODE_HEADER, node);

        let response = sender.send_request(request).await?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(super::rejected(response).await);
        }
        if !headers::has_token(response.headers(), UPGRADE, TUNNEL_PROTOCOL) {
            return Err(Error::Other(
                "server switched to a protocol other than mesh-tunnel/1".to_owned(),
            ));
        }
        hyper::upgrade::on(response).await.map_err(Error::from)
    }
}

/// Connect to the server and upgrade to a tunnel for `node`: the same as
/// [`Dial::connect`] followed by [`Dial::upgrade`]. Returns the
/// upgraded connection and the server's address.
///
/// Use [`Dial`] directly to check the server's identity before the
/// upgrade is sent.
#[allow(clippy::result_large_err)]
pub async fn dial(server_url: &Url, node: &str) -> Result<(Upgraded, SocketAddr), Error> {
    let dial = Dial::connect(server_url).await?;
    let peer = dial.peer_addr();
    Ok((dial.upgrade(node).await?, peer))
}

fn timed_out(step: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("timed out {step}"),
    ))
}
