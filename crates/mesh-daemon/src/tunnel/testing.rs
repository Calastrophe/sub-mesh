//! Test support: a local stand-in for `mesh-server`'s tunnel endpoint, and
//! requests sent over the tunnel the way the server sends them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use chrono::Utc;
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use mesh_core::Provider;
use mesh_core::protocol::TUNNEL_PATH;
use mesh_providers::TokenStore;
use mesh_relay::tunnel::accept;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::state::tests::{stored, temp_dir};
use crate::state::{AppState, RuntimeSettings};

/// Longest wait for something that should happen at once.
pub const WAIT: Duration = Duration::from_secs(5);

/// Where a test serves its fake tailscaled: a socket in `data_dir`, or on
/// Windows a named pipe.
#[cfg(unix)]
fn fake_tailscaled_address(_name: &str, data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("tailscaled.sock")
}

#[cfg(windows)]
fn fake_tailscaled_address(name: &str, _data_dir: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        r"\\.\pipe\mesh-daemon-{name}-{}",
        std::process::id()
    ))
}

/// A daemon state that dials `server_url`. `top` is extra top-level config,
/// `tables` is extra config tables, and `trust_loopback` sets
/// `allowed_peers.allow_loopback` (the test server is on loopback).
pub fn daemon_state(
    name: &str,
    server_url: &str,
    top: &str,
    tables: &str,
    trust_loopback: bool,
) -> Arc<AppState> {
    let data_dir = temp_dir(name);
    let socket = fake_tailscaled_address(name, &data_dir);
    let config: Config = format!(
        r#"
        server_url = "{server_url}"
        node = "test-node"
        tailscale_socket = {socket:?}
        {top}

        {tables}
        "#
    )
    .parse()
    .unwrap();
    let mut settings = config.resolve(None, "test").unwrap();
    settings.peers.allow_loopback |= trust_loopback;
    settings.data_dir = data_dir;
    let client = reqwest::Client::builder()
        .timeout(WAIT)
        .no_proxy()
        .build()
        .unwrap();
    AppState::new(settings, RuntimeSettings::default(), client)
}

/// A server that accepts tunnels on [`TUNNEL_PATH`] and hands each
/// `(node, client)` to the test.
pub struct TestServer {
    /// The server's base URL, for `server_url`.
    pub url: String,
    /// How many upgrade requests reached the server.
    pub upgrades: Arc<AtomicUsize>,
    clients: mpsc::Receiver<(String, mesh_relay::tunnel::Client)>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct ServerState {
    clients: mpsc::Sender<(String, mesh_relay::tunnel::Client)>,
    upgrades: Arc<AtomicUsize>,
}

impl TestServer {
    pub async fn start() -> TestServer {
        async fn handler(State(server): State<ServerState>, request: Request) -> Response {
            server.upgrades.fetch_add(1, Ordering::SeqCst);
            let accepted = match accept(request) {
                Ok(accepted) => accepted,
                Err(refusal) => return refusal.into_response(),
            };
            let node = accepted.node.clone();
            tokio::spawn(async move {
                let client = mesh_relay::tunnel::Client::from_upgrade(accepted.upgrade)
                    .await
                    .expect("tunnel handshake");
                let _ = server.clients.send((node, client)).await;
            });
            accepted.response
        }

        let (clients_tx, clients) = mpsc::channel(4);
        let upgrades = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(TUNNEL_PATH, get(handler))
            .with_state(ServerState {
                clients: clients_tx,
                upgrades: upgrades.clone(),
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TestServer {
            url,
            upgrades,
            clients,
            task,
        }
    }

    /// The next daemon to open a tunnel.
    pub async fn next_client(&mut self) -> (String, mesh_relay::tunnel::Client) {
        tokio::time::timeout(WAIT, self.clients.recv())
            .await
            .expect("a daemon opens a tunnel")
            .expect("test server is running")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A daemon running its tunnel task against a [`TestServer`].
pub struct Tunnel {
    pub client: mesh_relay::tunnel::Client,
    pub node: String,
    _server: TestServer,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Tunnel {
    /// Start [`super::run`] for `state` and wait until the server has the
    /// tunnel and the daemon reports it up. `state.settings.server_url` must
    /// be `server.url`.
    pub async fn connect(state: &Arc<AppState>, mut server: TestServer) -> Tunnel {
        let (shutdown, signal) = watch::channel(false);
        let task = tokio::spawn(super::run(state.clone(), signal));
        let (node, client) = server.next_client().await;
        tokio::time::timeout(WAIT, async {
            while state
                .admin_status(Utc::now())
                .tunnel_connected_since
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("daemon records the tunnel as up");
        Tunnel {
            client,
            node,
            _server: server,
            shutdown,
            task: Some(task),
        }
    }

    pub async fn with_codex_account(
        state: &Arc<AppState>,
        server: TestServer,
    ) -> (Self, TokenStore) {
        let store = TokenStore::new(state.settings.accounts_dir());
        store.save(&stored("codex-1", Provider::Codex)).unwrap();
        state.reload().await.unwrap();
        (Self::connect(state, server).await, store)
    }

    /// Ask the tunnel task to stop and wait for it.
    pub async fn shut_down(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(task) = self.task.take() {
            tokio::time::timeout(WAIT, task)
                .await
                .expect("tunnel task stops on shutdown")
                .unwrap();
        }
    }

    /// Start a request from the server's side of the tunnel.
    pub fn request(&self, method: Method, path: &str) -> Call {
        Call {
            client: self.client.clone(),
            method,
            path: path.to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// A request on the tunnel, not yet sent.
pub struct Call {
    client: mesh_relay::tunnel::Client,
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Call {
    /// Add a header; a repeated name adds another value.
    pub fn header(mut self, name: &str, value: &str) -> Call {
        self.headers.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Call {
        self.body = body.into();
        self
    }

    pub fn json(mut self, value: &impl Serialize) -> Call {
        self.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        self.body = Bytes::from(serde_json::to_vec(value).unwrap());
        self
    }

    pub async fn send(self) -> Reply {
        let response = self
            .client
            .send(self.method, &self.path, self.headers, self.body)
            .await
            .expect("request crosses the tunnel");
        Reply(response)
    }
}

/// The daemon's answer, with its body still streaming.
pub struct Reply(Response);

impl Reply {
    pub fn status(&self) -> StatusCode {
        self.0.status()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.0.headers()
    }

    pub async fn bytes(self) -> Bytes {
        axum::body::to_bytes(self.0.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    pub async fn text(self) -> String {
        String::from_utf8(self.bytes().await.to_vec()).unwrap()
    }

    pub async fn json<T: DeserializeOwned>(self) -> T {
        serde_json::from_slice(&self.bytes().await).unwrap()
    }
}
