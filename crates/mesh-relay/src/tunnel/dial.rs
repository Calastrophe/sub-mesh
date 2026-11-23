use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use http::header::{CONNECTION, HOST, UPGRADE};
use http::{HeaderValue, Request, StatusCode};
use http_body_util::Empty;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use mesh_core::protocol::{NODE_HEADER, TUNNEL_PATH, TUNNEL_PROTOCOL};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use url::Url;

use super::DIAL_TIMEOUT;
use crate::Error;
use crate::headers;

/// A byte stream to the server: plain TCP, or TLS over TCP.
pub trait ServerIo: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ServerIo for T {}

/// A connection to the server, made before the tunnel upgrade so the
/// daemon can check the server's identity before it sends anything: for
/// `http://` with tailnet whois on [`Dial::peer_addr`], for `https://` by
/// the server's certificate, which [`Dial::connect`] has already checked.
pub struct Dial {
    stream: Box<dyn ServerIo>,
    peer: SocketAddr,
    host: String,
    tls: bool,
}

impl std::fmt::Debug for Dial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Dial")
            .field("peer", &self.peer)
            .field("host", &self.host)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

/// The TLS settings for `https://` servers: the webpki roots, and only
/// HTTP/1.1 offered by ALPN, since the tunnel is an HTTP/1.1 upgrade that a
/// reverse proxy can carry, which HTTP/2 cannot.
fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the ring provider supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone()
}

impl Dial {
    /// Open a connection to `server_url`, within [`DIAL_TIMEOUT`] for the
    /// TCP connect and again for the TLS handshake.
    ///
    /// An `http://` URL gives a plain-text connection, which relies on the
    /// tailnet for encryption. An `https://` URL gives TLS checked against
    /// the webpki roots, for example to a reverse proxy in front of the
    /// server. The URL's path is ignored; the upgrade always goes to
    /// [`TUNNEL_PATH`].
    #[allow(clippy::result_large_err)]
    pub async fn connect(server_url: &Url) -> Result<Dial, Error> {
        let tls = match server_url.scheme() {
            "http" => false,
            "https" => true,
            _ => {
                return Err(Error::Other(format!(
                    "tunnel needs an http:// or https:// server URL, not {server_url}"
                )));
            }
        };
        let (Some(host), Some(port)) = (server_url.host_str(), server_url.port_or_known_default())
        else {
            return Err(Error::Other(format!(
                "server URL has no host: {server_url}"
            )));
        };
        let address = format!("{host}:{port}");
        let tcp = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(&address))
            .await
            .map_err(|_| timed_out("connecting to the server"))??;
        let peer = tcp.peer_addr()?;
        let stream: Box<dyn ServerIo> = if tls {
            // `host_str` keeps IPv6 brackets; a server name has none.
            let name = host.trim_start_matches('[').trim_end_matches(']');
            let name = ServerName::try_from(name.to_owned())
                .map_err(|err| Error::Other(format!("bad server name {name:?}: {err}")))?;
            let handshake = TlsConnector::from(tls_config()).connect(name, tcp);
            Box::new(
                tokio::time::timeout(DIAL_TIMEOUT, handshake)
                    .await
                    .map_err(|_| timed_out("in the TLS handshake with the server"))??,
            )
        } else {
            Box::new(tcp)
        };
        let host = match server_url.port() {
            Some(_) => address,
            None => host.to_owned(),
        };
        Ok(Dial {
            stream,
            peer,
            host,
            tls,
        })
    }

    /// Whether the connection is TLS, so the server's certificate has
    /// already been checked for the URL's host.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// The `Host` header value for requests on this connection.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The connection itself, for a plain HTTP/1.1 request such as a
    /// daemon report.
    pub fn into_io(self) -> Box<dyn ServerIo> {
        self.stream
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
