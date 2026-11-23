//! HTTP client for the daemon's loopback admin API.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mesh_core::ApiError;
use mesh_core::admin::{
    AdminStatus, MODELS_PATH, RELOAD_PATH, RESERVE_PATH, STATUS_PATH, SetModels, SetReserve,
};
use reqwest::{Client, Method, RequestBuilder};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the HTTP client used for provider requests.
///
/// Redirects are off so a redirect can never resend an OAuth token request
/// body to another host.
pub fn http_client() -> Result<Client> {
    http_builder()
        .build()
        .context("failed to build HTTP client")
}

fn http_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
}

/// The daemon refused the connection, so it is probably not running.
#[derive(Debug)]
struct DaemonNotRunning {
    url: String,
}

impl fmt::Display for DaemonNotRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mesh-daemon is not running at {}", self.url)
    }
}

impl std::error::Error for DaemonNotRunning {}

/// Whether the admin endpoint refused the connection.
pub fn is_not_running(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DaemonNotRunning>().is_some()
}

/// Client for the admin API at one base URL.
pub struct AdminClient {
    http: Client,
    base: String,
    token_path: PathBuf,
}

impl AdminClient {
    /// A client for the admin API at `base`. Every request sends the token in
    /// `token_path` (see [`mesh_core::admin`]), read when the request is made.
    pub fn new(base: &str, token_path: PathBuf) -> Result<Self> {
        Ok(AdminClient {
            http: http_builder()
                .no_proxy()
                .build()
                .context("failed to build daemon admin client")?,
            base: base.trim_end_matches('/').to_owned(),
            token_path,
        })
    }

    pub async fn status(&self) -> Result<AdminStatus> {
        self.call(Method::GET, STATUS_PATH).await
    }

    pub async fn reload(&self) -> Result<AdminStatus> {
        self.call(Method::POST, RELOAD_PATH).await
    }

    pub async fn call(&self, method: Method, path: &str) -> Result<AdminStatus> {
        self.send(self.request(method, path)?).await
    }

    pub async fn set_reserve(&self, body: &SetReserve) -> Result<AdminStatus> {
        self.send(self.request(Method::PUT, RESERVE_PATH)?.json(body))
            .await
    }

    pub async fn set_models(&self, body: &SetModels) -> Result<AdminStatus> {
        self.send(self.request(Method::PUT, MODELS_PATH)?.json(body))
            .await
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let token = mesh_providers::read_secret(&self.token_path).with_context(|| {
            let dir = self.token_path.parent().unwrap_or(&self.token_path);
            format!(
                "cannot read the admin token {}; is mesh-daemon running as this user? data dir: {}",
                self.token_path.display(),
                dir.display()
            )
        })?;
        Ok(self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token))
    }

    async fn send(&self, request: RequestBuilder) -> Result<AdminStatus> {
        let response = request
            .send()
            .await
            .map_err(|error| self.send_error(error))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json()
                .await
                .context("mesh-daemon sent an unreadable response");
        }
        let error = response.json::<ApiError>().await.with_context(|| {
            format!("mesh-daemon returned {status} with an unreadable error body")
        })?;
        bail!("mesh-daemon returned {status}: {}", error.error)
    }

    fn send_error(&self, error: reqwest::Error) -> anyhow::Error {
        if error.is_timeout() {
            anyhow::Error::new(error).context(format!("request to {} timed out", self.base))
        } else if error.is_connect()
            && anyhow::Chain::new(&error).any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::ConnectionRefused)
            })
        {
            anyhow::Error::new(DaemonNotRunning {
                url: self.base.clone(),
            })
        } else {
            anyhow::Error::new(error).context(format!("request to {} failed", self.base))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn token_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meshctl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(mesh_core::admin::TOKEN_FILE)
    }

    #[tokio::test]
    async fn refused_connection_means_not_running() {
        let path = token_path("refused");
        mesh_providers::read_or_create_secret(&path).unwrap();
        let client = AdminClient::new("http://127.0.0.1:1/", path.clone()).unwrap();
        let error = client.status().await.expect_err("nothing listens there");
        assert!(is_not_running(&error), "unexpected error: {error:#}");
        assert_eq!(
            error.to_string(),
            "mesh-daemon is not running at http://127.0.0.1:1"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn every_request_carries_the_bearer_token() {
        let path = token_path("bearer");
        let token = mesh_providers::read_or_create_secret(&path).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0; 4096];
                let mut len = 0;
                while !buffer[..len].windows(4).any(|window| window == b"\r\n\r\n") {
                    len += socket.read(&mut buffer[len..]).await.unwrap();
                }
                requests.push(String::from_utf8_lossy(&buffer[..len]).to_lowercase());
                let body = r#"{"error":"stop here"}"#;
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });

        let client = AdminClient::new(&base, path.clone()).unwrap();
        let error = client.status().await.expect_err("the server refuses");
        assert!(error.to_string().contains("stop here"), "{error:#}");
        client
            .call(Method::POST, "/admin/v1/reload")
            .await
            .unwrap_err();
        for request in server.await.unwrap() {
            assert!(
                request.contains(&format!(
                    "authorization: bearer {}\r\n",
                    token.to_lowercase()
                )),
                "{request}"
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_missing_token_file_is_a_clear_error() {
        let path = token_path("missing");
        let client = AdminClient::new("http://127.0.0.1:1", path.clone()).unwrap();
        let error = client.status().await.expect_err("no token file");
        assert!(!is_not_running(&error));
        let message = format!("{error:#}");
        assert!(
            message.contains("is mesh-daemon running as this user?"),
            "{message}"
        );
        assert!(
            message.contains(&format!("data dir: {}", path.parent().unwrap().display())),
            "{message}"
        );
    }
}
