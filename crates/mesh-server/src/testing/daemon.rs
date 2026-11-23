//! A fake daemon for tests. It dials the server over a real local tunnel,
//! as `mesh-daemon` does, and serves a router on it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use mesh_core::protocol::TUNNEL_PATH;
use mesh_relay::tunnel;
use serde::Serialize;
use tokio::task::JoinHandle;
use url::Url;

use super::serve_app;
use crate::AppState;

/// How long the tests wait for a tunnel to come up or go down.
const WAIT: Duration = Duration::from_secs(5);

/// The header that carries the caller's login to [`serve_tunnels`], since
/// whois cannot be faked over loopback.
pub const LOGIN_HEADER: &str = "x-test-login";

async fn tunnel_as_header(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let login = request
        .headers()
        .get(LOGIN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    crate::tunnel::connect(state, login, request)
}

/// Serve only the tunnel route. The peer's login comes from
/// [`LOGIN_HEADER`], and there is no whois check.
pub async fn serve_tunnels(state: Arc<AppState>) -> SocketAddr {
    serve_app(
        Router::new()
            .route(TUNNEL_PATH, get(tunnel_as_header))
            .with_state(state),
    )
    .await
}

/// The generation of the registry's current tunnel of `node`.
pub fn generation_of(state: &AppState, node: &str) -> Option<u64> {
    state.with_registry(|registry| registry.tunnel(node).map(|tunnel| tunnel.generation))
}

/// A daemon connected to a server by a tunnel.
pub struct FakeDaemon {
    serving: JoinHandle<()>,
    /// The generation the server gave the tunnel.
    pub generation: u64,
}

impl FakeDaemon {
    /// Dial `addr`, serve `router` on the tunnel, and wait until the server
    /// has registered it as the current tunnel of `node`.
    pub async fn dial(
        state: &Arc<AppState>,
        addr: SocketAddr,
        node: &str,
        router: Router,
    ) -> FakeDaemon {
        let before = generation_of(state, node);
        let url = Url::parse(&format!("http://{addr}")).unwrap();
        let (io, peer) = tunnel::dial(&url, node).await.expect("tunnel upgrade");
        let serving = tokio::spawn(async move {
            let _ = tunnel::serve(io, tunnel::with_connect_info(router, peer)).await;
        });
        let started = tokio::time::Instant::now();
        loop {
            let now = generation_of(state, node);
            if let Some(generation) = now.filter(|_| now != before) {
                return FakeDaemon {
                    serving,
                    generation,
                };
            }
            assert!(started.elapsed() < WAIT, "tunnel of {node} did not come up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Dial a server that serves only the tunnel route.
    pub async fn connect(state: &Arc<AppState>, node: &str, router: Router) -> FakeDaemon {
        let addr = serve_tunnels(state.clone()).await;
        FakeDaemon::dial(state, addr, node, router).await
    }

    /// Cut the tunnel without a goodbye, as a crashed daemon does.
    pub fn crash(&self) {
        self.serving.abort();
    }

    /// Wait until the daemon's side of the tunnel has ended.
    pub async fn closed(&mut self) {
        tokio::time::timeout(WAIT, &mut self.serving)
            .await
            .expect("tunnel did not close")
            .ok();
    }
}

/// Wait until `node` has no tunnel in the registry, or has one other than
/// `generation`.
pub async fn wait_for_detach(state: &AppState, node: &str, generation: u64) {
    let started = tokio::time::Instant::now();
    while generation_of(state, node) == Some(generation) {
        assert!(
            started.elapsed() < WAIT,
            "tunnel of {node} was not detached"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A canned answer.
#[derive(Clone, Debug)]
pub struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Bytes,
    delay: Duration,
}

impl Reply {
    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            headers: vec![],
            body: Bytes::new(),
            delay: Duration::ZERO,
        }
    }

    pub fn json(status: u16, value: &impl Serialize) -> Reply {
        Reply::status(status)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(value).unwrap())
    }

    pub fn header(mut self, name: &'static str, value: &str) -> Reply {
        self.headers.push((name, value.to_owned()));
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Reply {
        self.body = body.into();
        self
    }

    /// Wait this long, after the request arrived, before answering.
    pub fn delay(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }

    fn into_response(self) -> Response {
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = StatusCode::from_u16(self.status).unwrap();
        for (name, value) in self.headers {
            response.headers_mut().append(
                HeaderName::from_static(name),
                HeaderValue::from_str(&value).unwrap(),
            );
        }
        response
    }
}

/// A request the stub received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// A router that answers by method and path with canned replies and
/// remembers every request. An unknown route is a `404`.
#[derive(Default)]
pub struct Stub {
    replies: Mutex<Vec<(Method, String, Reply)>>,
    seen: Mutex<Vec<Seen>>,
}

impl Stub {
    pub fn new() -> Arc<Stub> {
        Arc::new(Stub::default())
    }

    /// Answer `method` requests for `path` with `reply`. A later call for
    /// the same route replaces the earlier reply.
    pub fn on(&self, method: Method, path: &str, reply: Reply) {
        let mut replies = self.replies.lock().unwrap();
        replies.retain(|(m, p, _)| !(*m == method && p == path));
        replies.push((method, path.to_owned(), reply));
    }

    /// Every request so far, oldest first.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// How many requests for `path` arrived, whatever the method.
    pub fn calls(&self, path: &str) -> usize {
        self.seen().iter().filter(|seen| seen.path == path).count()
    }

    /// The router to serve on a [`FakeDaemon`].
    pub fn router(self: &Arc<Stub>) -> Router {
        Router::new().fallback(answer).with_state(self.clone())
    }
}

async fn answer(State(stub): State<Arc<Stub>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match parts.method {
        Method::CONNECT => Bytes::new(),
        _ => axum::body::to_bytes(body, usize::MAX).await.unwrap(),
    };
    let path = parts.uri.path().to_owned();
    let reply = stub
        .replies
        .lock()
        .unwrap()
        .iter()
        .find(|(method, route, _)| *method == parts.method && *route == path)
        .map(|(_, _, reply)| reply.clone());
    stub.seen.lock().unwrap().push(Seen {
        method: parts.method,
        path,
        query: parts.uri.query().map(str::to_owned),
        headers: parts.headers,
        body,
    });
    let Some(reply) = reply else {
        return StatusCode::NOT_FOUND.into_response();
    };
    tokio::time::sleep(reply.delay).await;
    reply.into_response()
}
