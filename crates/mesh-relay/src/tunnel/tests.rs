use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::header::{CONNECTION, CONTENT_TYPE, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt;
use mesh_core::protocol::{DENIED_HEADER, HEADROOM_HEADER, TUNNEL_PATH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite;
use url::Url;

use super::{Client, Dial, accept, dial, serve, with_connect_info};
use crate::{Error, FrameHooks, MeshHeaders};

const CONCURRENT: usize = 8;
const WAIT: Duration = Duration::from_secs(5);

/// Server end: an axum app on `TUNNEL_PATH` that accepts tunnels and hands
/// each `(node, client)` to the test. Node `intruder` is refused with `403`.
async fn tunnel_server() -> (Url, mpsc::Receiver<(String, Client)>) {
    async fn handler(
        State(clients): State<mpsc::Sender<(String, Client)>>,
        request: Request,
    ) -> Response {
        let accepted = match accept(request) {
            Ok(accepted) => accepted,
            Err(refusal) => return refusal.into_response(),
        };
        if accepted.node == "intruder" {
            return (StatusCode::FORBIDDEN, "not on the daemons list\n").into_response();
        }
        let node = accepted.node;
        tokio::spawn(async move {
            let client = Client::from_upgrade(accepted.upgrade)
                .await
                .expect("tunnel handshake");
            let _ = clients.send((node, client)).await;
        });
        accepted.response
    }

    let (clients_tx, clients_rx) = mpsc::channel(4);
    let app = Router::new()
        .route(TUNNEL_PATH, get(handler))
        .with_state(clients_tx);
    let url = spawn_app(app).await;
    (url, clients_rx)
}

/// Serve `app` on a fresh loopback port and return its base URL.
async fn spawn_app(app: Router) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Url::parse(&format!("http://{address}")).unwrap()
}

/// The daemon's router used by the tests.
fn daemon_router(barrier: Arc<Barrier>) -> Router {
    async fn echo(ConnectInfo(peer): ConnectInfo<SocketAddr>, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut seen: Vec<&str> = parts.headers.keys().map(|name| name.as_str()).collect();
        seen.sort_unstable();
        let chunks = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"echo:")),
            Ok(body),
        ];
        let mut response = Response::new(Body::from_stream(futures_util::stream::iter(chunks)));
        let headers = response.headers_mut();
        headers.insert("x-peer", peer.to_string().parse().unwrap());
        headers.insert("x-seen", seen.join(",").parse().unwrap());
        headers.insert(
            "x-uri",
            parts
                .uri
                .path_and_query()
                .unwrap()
                .as_str()
                .parse()
                .unwrap(),
        );
        headers.insert(HEADROOM_HEADER, HeaderValue::from_static("0.5"));
        headers.append("x-multi", HeaderValue::from_static("a"));
        headers.append("x-multi", HeaderValue::from_static("b"));
        response
    }

    async fn rendezvous(State(barrier): State<Arc<Barrier>>) -> &'static str {
        barrier.wait().await;
        "met"
    }

    async fn websocket(upgrade: WebSocketUpgrade, request: Request) -> Response {
        if !crate::is_websocket_request(&request.into_parts().0) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        upgrade
            .protocols(["chat"])
            .on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    match message {
                        Message::Text(_) | Message::Binary(_) => {
                            if socket.send(message).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) => {}
                    }
                }
            })
    }

    async fn denied() -> Response {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(DENIED_HEADER, "reserve")],
            "denied\n",
        )
            .into_response()
    }

    Router::new()
        .route("/echo", post(echo))
        .route("/rendezvous", get(rendezvous))
        .route("/ws", any(websocket))
        .route("/ws-denied", any(denied))
        .with_state(barrier)
}

/// A connected tunnel: the server's client and the daemon's serve task.
struct Tunnel {
    client: Client,
    daemon: JoinHandle<Result<(), Error>>,
    server_addr: SocketAddr,
}

async fn open_tunnel() -> Tunnel {
    let (url, mut clients) = tunnel_server().await;
    let (io, server_addr) = dial(&url, "box").await.expect("dial");
    let router = with_connect_info(
        daemon_router(Arc::new(Barrier::new(CONCURRENT))),
        server_addr,
    );
    let daemon = tokio::spawn(serve(io, router));
    let (node, client) = tokio::time::timeout(WAIT, clients.recv())
        .await
        .expect("tunnel in time")
        .expect("tunnel accepted");
    assert_eq!(node, "box");
    Tunnel {
        client,
        daemon,
        server_addr,
    }
}

#[tokio::test]
async fn post_streams_response_with_headers() {
    let tunnel = open_tunnel().await;
    let mut consumer = HeaderMap::new();
    consumer.insert("x-in", HeaderValue::from_static("hello"));
    consumer.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    consumer.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    consumer.insert("keep-alive", HeaderValue::from_static("timeout=5"));
    consumer.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
    consumer.insert(UPGRADE, HeaderValue::from_static("h2c"));
    consumer.insert("te", HeaderValue::from_static("gzip"));

    let response = tunnel
        .client
        .send(
            Method::POST,
            "/echo?q=1",
            consumer,
            Bytes::from_static(b"payload"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-peer"], tunnel.server_addr.to_string());
    assert_eq!(response.headers()["x-uri"], "/echo?q=1");
    let seen = response.headers()["x-seen"].to_str().unwrap().to_owned();
    for name in seen.split(',') {
        assert!(
            matches!(name, "x-in" | "content-type" | "content-length"),
            "daemon saw unexpected header {name} in {seen}"
        );
    }
    assert!(seen.contains("x-in"), "x-in missing: {seen}");

    let mut extra = HeaderMap::new();
    extra.insert("x-extra", HeaderValue::from_static("1"));
    let relayed = crate::forward_response(response, MeshHeaders::Strip, extra);
    assert!(relayed.headers().get(HEADROOM_HEADER).is_none());
    assert_eq!(relayed.headers()["x-extra"], "1");
    let multi: Vec<_> = relayed.headers().get_all("x-multi").iter().collect();
    assert_eq!(multi, ["a", "b"]);
    let body = relayed.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, "echo:payload");
}

#[tokio::test]
async fn concurrent_requests_are_multiplexed() {
    let tunnel = open_tunnel().await;
    let calls = (0..CONCURRENT).map(|_| {
        let client = tunnel.client.clone();
        async move {
            let response = client
                .send(Method::GET, "/rendezvous", HeaderMap::new(), Bytes::new())
                .await
                .expect("send");
            response.into_body().collect().await.unwrap().to_bytes()
        }
    });
    let bodies = tokio::time::timeout(WAIT, futures_util::future::join_all(calls))
        .await
        .expect("all requests must be in flight together");
    assert!(bodies.iter().all(|body| body == "met"));
}

#[tokio::test]
async fn websocket_relay_echoes_and_ends_when_the_consumer_closes() {
    let tunnel = open_tunnel().await;
    let (guard_tx, mut guard_rx) = mpsc::channel::<oneshot::Receiver<()>>(1);
    let client = tunnel.client.clone();
    let front = Router::new().route(
        "/ws",
        get(move |upgrade: WebSocketUpgrade, request: Request| {
            let client = client.clone();
            let guard_tx = guard_tx.clone();
            async move {
                let socket = match client
                    .connect_websocket("/ws", request.headers().clone())
                    .await
                {
                    Ok(socket) => socket,
                    Err(err) => return err.into_response(),
                };
                assert_eq!(socket.response_headers()["sec-websocket-protocol"], "chat");
                let (done_tx, done_rx) = oneshot::channel();
                guard_tx.send(done_rx).await.unwrap();
                upgrade
                    .protocols(["chat"])
                    .on_upgrade(|consumer| async move {
                        socket.relay(consumer, FrameHooks::default()).await;
                        drop(done_tx);
                    })
            }
        }),
    );
    let front_url = spawn_app(front).await;

    use tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{}/ws", front_url.authority())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", HeaderValue::from_static("chat"));
    let (mut consumer, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "chat");
    let mut done = guard_rx.recv().await.unwrap();

    consumer
        .send(tungstenite::Message::Text("ping over tunnel".into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(WAIT, consumer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echoed.into_text().unwrap().as_str(), "ping over tunnel");
    assert!(
        matches!(done.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "the relay must run until a side closes"
    );

    consumer.close(None).await.unwrap();
    tokio::time::timeout(WAIT, done)
        .await
        .expect("relay ended after close")
        .expect_err("the relay only signals by dropping");
}

#[tokio::test]
async fn refused_websocket_is_rejected_with_headers() {
    let tunnel = open_tunnel().await;
    let err = tunnel
        .client
        .connect_websocket("/ws-denied", HeaderMap::new())
        .await
        .err()
        .expect("refusal");
    let Error::Rejected {
        status,
        headers,
        body,
    } = err
    else {
        panic!("expected Rejected, got {err}");
    };
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers[DENIED_HEADER], "reserve");
    assert_eq!(body, "denied\n");
}

#[tokio::test]
async fn dropped_daemon_closes_tunnel() {
    let tunnel = open_tunnel().await;
    assert!(!tunnel.client.is_closed());
    let closed = tunnel.client.closed();
    tunnel.daemon.abort();
    tokio::time::timeout(WAIT, closed)
        .await
        .expect("closed() resolves");
    assert!(tunnel.client.is_closed());
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        tunnel
            .client
            .send(Method::GET, "/rendezvous", HeaderMap::new(), Bytes::new()),
    )
    .await
    .expect("send fails promptly");
    let error = result.expect_err("a closed tunnel cannot send");
    assert!(matches!(error, Error::Tunnel(_)), "{error:?}");
    assert!(error.never_dispatched(), "{error:?}");
    let error = tunnel
        .client
        .connect_websocket("/rendezvous", HeaderMap::new())
        .await
        .err()
        .expect("a closed tunnel cannot connect");
    assert!(error.never_dispatched(), "{error:?}");
}

#[tokio::test]
async fn wrong_upgrade_token_is_refused() {
    let (url, _clients) = tunnel_server().await;
    let mut stream = TcpStream::connect(url.authority()).await.unwrap();
    stream
        .write_all(
            format!(
                "GET {TUNNEL_PATH} HTTP/1.1\r\nHost: {}\r\nConnection: upgrade\r\nUpgrade: websocket\r\nx-mesh-node: box\r\n\r\n",
                url.authority()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut status = String::new();
    tokio::time::timeout(WAIT, BufReader::new(stream).read_line(&mut status))
        .await
        .unwrap()
        .unwrap();
    assert!(status.starts_with("HTTP/1.1 426"), "{status}");
}

#[tokio::test]
async fn refused_dial_is_rejected() {
    let (url, _clients) = tunnel_server().await;
    let dial = Dial::connect(&url).await.unwrap();
    assert_eq!(dial.peer_addr().to_string(), url.authority());
    let err = dial.upgrade("intruder").await.expect_err("refusal");
    assert!(
        matches!(
            err,
            Error::Rejected {
                status: StatusCode::FORBIDDEN,
                ..
            }
        ),
        "{err}"
    );
}
