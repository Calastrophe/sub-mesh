//! Minimal client for the tailscaled LocalAPI. On Unix it talks over a Unix
//! socket, and on Windows over a named pipe.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::Request;
use http::header::HOST;
use http_body_util::{BodyExt, Empty, Limited};
use hyper_util::rt::TokioIo;
use serde::Deserialize;

/// The `Host` that tailscaled expects on LocalAPI requests.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";

const LOCALAPI_TIMEOUT: Duration = Duration::from_secs(5);
const LOCALAPI_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// Default tailscaled socket path on Linux.
#[cfg(unix)]
pub const DEFAULT_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// Default tailscaled LocalAPI named pipe on Windows.
#[cfg(windows)]
pub const DEFAULT_SOCKET: &str = r"\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled";

/// The identity of a tailnet peer, from `/localapi/v0/whois`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhoIs {
    /// The owning user's login, for example `alice@example.com`. For tagged
    /// nodes this is `tagged-devices`.
    pub login_name: String,
    /// The peer node's host name.
    pub node_name: String,
    /// ACL tags on the node, such as `tag:mesh`.
    pub tags: Vec<String>,
}

/// This machine's own tailnet identity, from `/localapi/v0/status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfStatus {
    pub login_name: String,
    pub host_name: String,
    /// Tailnet IPs (100.x and fd7a:... addresses).
    pub addresses: Vec<IpAddr>,
}

/// Errors from the LocalAPI.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot reach tailscaled at {path}: {source}")]
    Connect {
        path: String,
        source: std::io::Error,
    },
    #[error("tailscaled returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("invalid LocalAPI response: {0}")]
    Decode(String),
}

/// Client for the tailscaled LocalAPI.
#[derive(Clone, Debug)]
pub struct LocalClient {
    socket: PathBuf,
}

impl LocalClient {
    /// Use the tailscaled LocalAPI at this Unix socket path, or on Windows
    /// this named pipe (`\\.\pipe\...`).
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        LocalClient {
            socket: socket.into(),
        }
    }

    /// Look up the peer behind a remote address.
    pub async fn whois(&self, addr: SocketAddr) -> Result<WhoIs, Error> {
        let query: String =
            url::form_urlencoded::byte_serialize(addr.to_string().as_bytes()).collect();
        let raw: RawWhoIs = self
            .get(&format!("/localapi/v0/whois?addr={query}"))
            .await?;
        let node_name = if raw.node.computed_name.is_empty() {
            raw.node
                .name
                .split('.')
                .next()
                .unwrap_or_default()
                .to_owned()
        } else {
            raw.node.computed_name
        };
        if raw.user_profile.login_name.trim().is_empty() || node_name.trim().is_empty() {
            return Err(Error::Decode(
                "whois response is missing the peer login or node name".to_owned(),
            ));
        }
        Ok(WhoIs {
            login_name: raw.user_profile.login_name,
            node_name,
            tags: raw.node.tags.unwrap_or_default(),
        })
    }

    /// This machine's identity and tailnet addresses.
    pub async fn self_status(&self) -> Result<SelfStatus, Error> {
        let mut raw: RawStatus = self.get("/localapi/v0/status").await?;
        let login_name = raw
            .user
            .remove(&raw.this.user_id.to_string())
            .map(|user| user.login_name)
            .unwrap_or_default();
        Ok(SelfStatus {
            login_name,
            host_name: raw.this.host_name,
            addresses: raw.this.tailscale_ips,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        tokio::time::timeout(LOCALAPI_TIMEOUT, self.get_inner(path))
            .await
            .map_err(|_| self.connect_error(std::io::ErrorKind::TimedOut.into()))?
    }

    async fn get_inner<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let stream = connect(&self.socket)
            .await
            .map_err(|err| self.connect_error(err))?;
        let exchange_error = |err| self.connect_error(std::io::Error::other(err));
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(exchange_error)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let request = Request::get(path)
            .header(HOST, LOCALAPI_HOST)
            .body(Empty::<Bytes>::new())
            .map_err(|err| Error::Decode(err.to_string()))?;
        let response = sender.send_request(request).await.map_err(exchange_error)?;
        let status = response.status();
        let body = Limited::new(response.into_body(), LOCALAPI_BODY_LIMIT)
            .collect()
            .await
            .map_err(|err| Error::Decode(format!("cannot read LocalAPI body: {err}")))?
            .to_bytes();
        if status.is_success() {
            serde_json::from_slice(&body).map_err(|err| Error::Decode(err.to_string()))
        } else {
            Err(Error::Status {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).trim().to_owned(),
            })
        }
    }

    fn connect_error(&self, source: std::io::Error) -> Error {
        Error::Connect {
            path: self.socket.display().to_string(),
            source,
        }
    }
}

#[cfg(unix)]
async fn connect(path: &Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path).await
}

/// How often to retry a pipe whose server has no free instance.
#[cfg(windows)]
const PIPE_BUSY_TRIES: u32 = 40;

/// How long to wait between those tries.
#[cfg(windows)]
const PIPE_BUSY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(windows)]
async fn connect(path: &Path) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    loop {
        match ClientOptions::new().open(path) {
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < PIPE_BUSY_TRIES =>
            {
                tries += 1;
                tokio::time::sleep(PIPE_BUSY_DELAY).await;
            }
            result => return result,
        }
    }
}

#[derive(Deserialize)]
struct RawWhoIs {
    #[serde(rename = "Node", default)]
    node: RawNode,
    #[serde(rename = "UserProfile", default)]
    user_profile: RawUserProfile,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct RawNode {
    name: String,
    computed_name: String,
    tags: Option<Vec<String>>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct RawUserProfile {
    login_name: String,
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "Self")]
    this: RawSelf,
    #[serde(rename = "User", default)]
    user: HashMap<String, RawUserProfile>,
}

#[derive(Deserialize)]
struct RawSelf {
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
    #[serde(rename = "UserID", default)]
    user_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(unix)]
    fn fake_address(name: impl std::fmt::Display) -> PathBuf {
        std::env::temp_dir().join(format!("mesh-relay-ts-{}-{name}.sock", std::process::id()))
    }

    #[cfg(windows)]
    fn fake_address(name: impl std::fmt::Display) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\mesh-relay-ts-{}-{name}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    fn listen(path: &Path) -> tokio::net::UnixListener {
        let _ = std::fs::remove_file(path);
        tokio::net::UnixListener::bind(path).unwrap()
    }

    #[cfg(unix)]
    async fn accept(listener: tokio::net::UnixListener) -> tokio::net::UnixStream {
        listener.accept().await.unwrap().0
    }

    #[cfg(windows)]
    fn listen(path: &Path) -> tokio::net::windows::named_pipe::NamedPipeServer {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(path)
            .unwrap()
    }

    #[cfg(windows)]
    async fn accept(
        server: tokio::net::windows::named_pipe::NamedPipeServer,
    ) -> tokio::net::windows::named_pipe::NamedPipeServer {
        server.connect().await.unwrap();
        server
    }

    /// Serve one canned response on a fresh socket (a named pipe on Windows);
    /// returns its address and a handle that yields the raw request.
    fn fake_tailscaled(
        status_line: &'static str,
        body: impl AsRef<str> + Send + 'static,
    ) -> (PathBuf, tokio::task::JoinHandle<String>) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = fake_address(NEXT.fetch_add(1, Ordering::Relaxed));
        let listener = listen(&path);
        let cleanup = path.clone();
        let handle = tokio::spawn(async move {
            let mut stream = accept(listener).await;
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "client closed early");
                request.extend_from_slice(&buf[..n]);
            }
            let body = body.as_ref();
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = std::fs::remove_file(cleanup);
            let _ = stream.read(&mut buf).await;
            String::from_utf8(request).unwrap()
        });
        (path, handle)
    }

    #[tokio::test]
    async fn whois_uses_computed_name_and_tags() {
        let (path, request) = fake_tailscaled(
            "200 OK",
            r#"{"Node":{"Name":"box.tail1234.ts.net.","ComputedName":"box","Tags":["tag:mesh"]},"UserProfile":{"LoginName":"alice@example.com"},"Extra":1}"#,
        );
        let who = LocalClient::new(path)
            .whois("100.64.0.7:5555".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            who,
            WhoIs {
                login_name: "alice@example.com".into(),
                node_name: "box".into(),
                tags: vec!["tag:mesh".into()],
            }
        );
        let request = request.await.unwrap().to_lowercase();
        assert!(request.starts_with("get /localapi/v0/whois?addr=100.64.0.7%3a5555 http/1.1\r\n"));
        assert!(request.contains("\r\nhost: local-tailscaled.sock\r\n"));
    }

    #[tokio::test]
    async fn whois_falls_back_to_first_label_and_null_tags() {
        let (path, request) = fake_tailscaled(
            "200 OK",
            r#"{"Node":{"Name":"box.tail1234.ts.net.","Tags":null},"UserProfile":{"LoginName":"tagged-devices"}}"#,
        );
        let who = LocalClient::new(path)
            .whois("[fd7a:115c:a1e0::1]:80".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(who.node_name, "box");
        assert!(who.tags.is_empty());
        assert!(
            request
                .await
                .unwrap()
                .contains("addr=%5Bfd7a%3A115c%3Aa1e0%3A%3A1%5D%3A80")
        );
    }

    #[tokio::test]
    async fn whois_rejects_incomplete_identity() {
        for body in [
            r#"{"Node":{"Name":"box.tail1234.ts.net."}}"#,
            r#"{"UserProfile":{"LoginName":"alice@example.com"}}"#,
        ] {
            let (path, _request) = fake_tailscaled("200 OK", body);
            let result = LocalClient::new(path)
                .whois("100.64.0.7:5555".parse().unwrap())
                .await;
            assert!(matches!(result, Err(Error::Decode(_))));
        }
    }

    #[tokio::test]
    async fn self_status_resolves_login_from_user_table() {
        let (path, _request) = fake_tailscaled(
            "200 OK",
            r#"{"Self":{"HostName":"ganymede","TailscaleIPs":["100.69.152.3","fd7a:115c:a1e0::992c:9804"],"UserID":42},"User":{"7":{"LoginName":"bob@example.com"},"42":{"LoginName":"alice@example.com"}}}"#,
        );
        let status = LocalClient::new(path).self_status().await.unwrap();
        assert_eq!(status.login_name, "alice@example.com");
        assert_eq!(status.host_name, "ganymede");
        assert_eq!(
            status.addresses,
            vec![
                "100.69.152.3".parse::<IpAddr>().unwrap(),
                "fd7a:115c:a1e0::992c:9804".parse().unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn localapi_bounds_response_body() {
        let (path, _request) =
            fake_tailscaled("403 Forbidden", " ".repeat(LOCALAPI_BODY_LIMIT + 1));
        let error = LocalClient::new(path).self_status().await.unwrap_err();
        assert!(matches!(error, Error::Decode(_)));
    }

    #[tokio::test]
    async fn localapi_times_out_when_the_socket_stalls() {
        let path = fake_address("stall");
        let listener = listen(&path);
        let client = LocalClient::new(&path);
        let stalled = async {
            let _stream = accept(listener).await;
            let _ = std::fs::remove_file(&path);
            std::future::pending::<()>().await;
        };
        let error = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                result = client.self_status() => result.unwrap_err(),
                () = stalled => unreachable!(),
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            error,
            Error::Connect { source, .. }
                if source.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[tokio::test]
    async fn error_statuses_and_bad_bodies() {
        let (path, _request) = fake_tailscaled("403 Forbidden", "access denied\n");
        let err = LocalClient::new(path).self_status().await.unwrap_err();
        assert!(matches!(err, Error::Status { status: 403, ref body } if body == "access denied"));

        let (path, _request) = fake_tailscaled("200 OK", "not json");
        let err = LocalClient::new(path).self_status().await.unwrap_err();
        assert!(matches!(err, Error::Decode(_)));

        let err = LocalClient::new(fake_address("missing"))
            .self_status()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Connect { .. }));
    }
}
