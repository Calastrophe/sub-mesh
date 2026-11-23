use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{self, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use mesh_relay::{
    ClientText, FrameHooks, MeshHeaders, UpstreamSocket, forward_request_headers, forward_response,
    forward_response_headers, into_response, is_websocket_upgrade, send,
};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message as UpstreamMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use url::Url;

fn map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

fn pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_owned()))
        .collect()
}

async fn serve(app: Router) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[test]
fn request_header_filtering() {
    let input = map(&[
        ("host", "mesh.local"),
        ("content-length", "12"),
        ("user-agent", "claude-cli/1.0"),
        ("accept-encoding", "gzip, br"),
        ("x-api-key", "k"),
        ("anthropic-beta", "a"),
        ("anthropic-beta", "b"),
        ("connection", "keep-alive, X-Private"),
        ("x-private", "secret"),
        ("keep-alive", "timeout=5"),
        ("te", "trailers"),
        ("trailer", "x"),
        ("transfer-encoding", "chunked"),
        ("upgrade", "h2c"),
        ("proxy-authorization", "Basic x"),
        ("proxy-connection", "keep-alive"),
        ("x-mesh-account", "1"),
        ("x-last", "z"),
    ]);
    let kept = forward_request_headers(&input, MeshHeaders::Keep);
    assert_eq!(
        pairs(&kept),
        [
            ("user-agent", "claude-cli/1.0"),
            ("accept-encoding", "gzip, br"),
            ("x-api-key", "k"),
            ("anthropic-beta", "a"),
            ("anthropic-beta", "b"),
            ("x-mesh-account", "1"),
            ("x-last", "z"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
    let stripped = forward_request_headers(&input, MeshHeaders::Strip);
    assert!(!stripped.contains_key("x-mesh-account"));
    assert_eq!(stripped.len(), kept.len() - 1);
}

#[test]
fn connection_tokens_are_parsed_individually() {
    let mut input = map(&[("x-private", "secret"), ("x-keep", "visible")]);
    input.append(
        "connection",
        HeaderValue::from_bytes(b"bad-\xff, X-Private").unwrap(),
    );
    let forwarded = forward_request_headers(&input, MeshHeaders::Keep);
    assert_eq!(
        pairs(&forwarded),
        [("x-keep".to_owned(), "visible".to_owned())]
    );
}

#[tokio::test]
async fn tunnel_response_forwards_data_without_trailers() {
    let frames = futures_util::stream::iter([
        Ok::<_, std::io::Error>(hyper::body::Frame::data(Bytes::from_static(b"payload"))),
        Ok(hyper::body::Frame::trailers(map(&[(
            "x-mesh-headroom",
            "0.5",
        )]))),
    ]);
    let upstream = Response::new(Body::new(StreamBody::new(frames)));

    let response = forward_response(upstream, MeshHeaders::Strip, HeaderMap::new());
    let body = response.into_body().collect().await.unwrap();
    assert!(body.trailers().is_none());
    assert_eq!(body.to_bytes(), "payload");
}

#[test]
fn response_header_filtering() {
    let input = map(&[
        ("content-type", "text/event-stream"),
        ("content-length", "5"),
        ("set-cookie", "a=1"),
        ("set-cookie", "b=2"),
        ("connection", "x-hop"),
        ("x-hop", "1"),
        ("transfer-encoding", "chunked"),
        ("x-mesh-headroom", "0.5"),
        ("x-request-id", "r"),
    ]);
    let kept = forward_response_headers(&input, MeshHeaders::Keep);
    assert_eq!(
        pairs(&kept),
        [
            ("content-type", "text/event-stream"),
            ("set-cookie", "a=1"),
            ("set-cookie", "b=2"),
            ("x-mesh-headroom", "0.5"),
            ("x-request-id", "r"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
    assert!(!forward_response_headers(&input, MeshHeaders::Strip).contains_key("x-mesh-headroom"));
}

#[test]
fn detects_websocket_upgrade() {
    assert!(is_websocket_upgrade(&map(&[
        ("connection", "keep-alive, Upgrade"),
        ("upgrade", "WebSocket"),
    ])));
    assert!(!is_websocket_upgrade(&map(&[("upgrade", "websocket")])));
    assert!(!is_websocket_upgrade(&map(&[
        ("connection", "upgrade"),
        ("upgrade", "h2c"),
    ])));
    assert!(!is_websocket_upgrade(&HeaderMap::new()));
}

#[tokio::test]
async fn send_and_stream_round_trip() {
    #[derive(Clone, Default)]
    struct Upstream {
        seen: Arc<Mutex<Option<(Method, HeaderMap, Bytes)>>>,
        gate: Arc<Notify>,
    }
    async fn handler(
        State(state): State<Upstream>,
        method: Method,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        *state.seen.lock().unwrap() = Some((method, headers, body));
        let gate = state.gate.clone();
        let stream = futures_util::stream::unfold(0, move |step| {
            let gate = gate.clone();
            async move {
                match step {
                    0 => Some((Ok::<_, std::io::Error>(Bytes::from("one ")), 1)),
                    1 => {
                        gate.notified().await;
                        Some((Ok(Bytes::from("two")), 2))
                    }
                    _ => None,
                }
            }
        });
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = StatusCode::CREATED;
        let headers = response.headers_mut();
        headers.append("x-mesh-headroom", HeaderValue::from_static("upstream"));
        headers.append("x-first", HeaderValue::from_static("1"));
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("x-second", HeaderValue::from_static("2"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));
        response
    }

    let state = Upstream::default();
    let addr = serve(
        Router::new()
            .route("/v1/x", any(handler))
            .with_state(state.clone()),
    )
    .await;

    let request_headers = map(&[
        ("x-zeta", "1"),
        ("authorization", "Bearer t"),
        ("x-alpha", "2"),
        ("x-alpha", "3"),
    ]);
    let upstream = send(
        &client(),
        Method::POST,
        format!("http://{addr}/v1/x").parse().unwrap(),
        request_headers,
        Bytes::from_static(b"payload"),
    )
    .await
    .unwrap();

    let response = into_response(
        upstream,
        MeshHeaders::Keep,
        map(&[("x-mesh-headroom", "0.25")]),
    );
    assert_eq!(response.status(), StatusCode::CREATED);
    let got: Vec<_> = pairs(response.headers())
        .into_iter()
        .filter(|(name, _)| name != "date")
        .collect();
    assert_eq!(
        got,
        [
            ("x-first", "1"),
            ("set-cookie", "a=1"),
            ("set-cookie", "b=2"),
            ("x-second", "2"),
            ("x-mesh-headroom", "0.25"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );

    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("first chunk was buffered")
        .unwrap()
        .unwrap();
    assert_eq!(first, "one ");
    state.gate.notify_one();
    let second = body.next().await.unwrap().unwrap();
    assert_eq!(second, "two");
    assert!(body.next().await.is_none());

    let (method, seen, payload) = state.seen.lock().unwrap().take().unwrap();
    assert_eq!(method, Method::POST);
    assert_eq!(payload, "payload");
    assert!(!seen.contains_key(USER_AGENT));
    assert!(!seen.contains_key(ACCEPT_ENCODING));
    let custom: Vec<_> = pairs(&seen)
        .into_iter()
        .filter(|(name, _)| name.starts_with("x-") || name == "authorization")
        .collect();
    assert_eq!(
        custom,
        [
            ("x-zeta", "1"),
            ("authorization", "Bearer t"),
            ("x-alpha", "2"),
            ("x-alpha", "3"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
}

#[tokio::test]
async fn bodiless_get_sends_no_content_length_and_keeps_accept() {
    async fn echo(headers: HeaderMap) -> String {
        format!(
            "{}|{}",
            headers.contains_key(CONTENT_LENGTH),
            headers
                .get(ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
        )
    }
    let addr = serve(Router::new().route("/", get(echo))).await;
    let upstream = send(
        &client(),
        Method::GET,
        format!("http://{addr}/").parse().unwrap(),
        map(&[("accept", "text/event-stream")]),
        Bytes::new(),
    )
    .await
    .unwrap();
    assert_eq!(upstream.text().await.unwrap(), "false|text/event-stream");
}

async fn echo_ws(upgrade: WebSocketUpgrade, headers: HeaderMap) -> Response {
    let token = headers
        .get("x-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("none")
        .to_owned();
    upgrade
        .protocols(["mesh"])
        .on_upgrade(move |mut socket| async move {
            let _ = socket
                .send(ws::Message::Text(format!("hello {token}").into()))
                .await;
            while let Some(Ok(message)) = socket.recv().await {
                match message {
                    ws::Message::Text(_) | ws::Message::Binary(_) => {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                    ws::Message::Close(_) => break,
                    _ => {}
                }
            }
        })
}

async fn relay_ws(
    State(upstream): State<Url>,
    upgrade: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    let headers = forward_request_headers(&headers, MeshHeaders::Strip);
    match UpstreamSocket::connect(upstream, headers).await {
        Ok(socket) => socket.bridge(
            upgrade,
            map(&[("x-mesh-headroom", "0.5")]),
            FrameHooks::default(),
        ),
        Err(err) => err.into_response(),
    }
}

type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(client: &mut Client) -> UpstreamMessage {
    tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn websocket_pre_bridge_send_ignores_close() {
    let addr = serve(Router::new().route("/ws", get(echo_ws))).await;
    let mut socket =
        UpstreamSocket::connect(format!("ws://{addr}/ws").parse().unwrap(), HeaderMap::new())
            .await
            .unwrap();

    socket.send(ws::Message::Close(None)).await.unwrap();
    socket.send(ws::Message::Text("data".into())).await.unwrap();
}

#[tokio::test]
async fn websocket_handshake_times_out_when_upstream_stalls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let open = async {
        UpstreamSocket::connect(url, HeaderMap::new())
            .await
            .err()
            .unwrap()
    };
    let stalled = async {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    };
    let error = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::select! {
            error = open => error,
            () = stalled => unreachable!(),
        }
    })
    .await
    .unwrap();
    assert!(error.to_string().contains("handshake timed out"));
}

#[tokio::test]
async fn websocket_bridge_echoes() {
    let upstream_addr = serve(Router::new().route("/ws", get(echo_ws))).await;
    let upstream_url: Url = format!("ws://{upstream_addr}/ws").parse().unwrap();
    let relay_addr = serve(
        Router::new()
            .route("/ws", get(relay_ws))
            .with_state(upstream_url),
    )
    .await;

    let mut request = format!("ws://{relay_addr}/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", HeaderValue::from_static("mesh"));
    request
        .headers_mut()
        .insert("x-token", HeaderValue::from_static("abc"));
    let (mut client, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "mesh");
    assert_eq!(response.headers()["x-mesh-headroom"], "0.5");

    assert_eq!(
        next(&mut client).await,
        UpstreamMessage::Text("hello abc".into())
    );
    client
        .send(UpstreamMessage::Text("ping".into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut client).await,
        UpstreamMessage::Text("ping".into())
    );
    client
        .send(UpstreamMessage::Binary(Bytes::from_static(&[1, 2, 3])))
        .await
        .unwrap();
    assert_eq!(
        next(&mut client).await,
        UpstreamMessage::Binary(Bytes::from_static(&[1, 2, 3]))
    );

    let close = UpstreamMessage::Close(Some(tungstenite::protocol::CloseFrame {
        code: tungstenite::protocol::frame::coding::CloseCode::Away,
        reason: "client going away".into(),
    }));
    client.send(close.clone()).await.unwrap();
    assert_eq!(next(&mut client).await, close);
}

/// Relay to `upstream` after sending it `first`, answering client texts
/// that start with `refuse`, dropping client binary messages, and
/// upper-casing upstream texts that start with `hello`.
async fn relay_ws_with_hooks(State(upstream): State<Url>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |client| async move {
        let mut socket = UpstreamSocket::connect(upstream, HeaderMap::new())
            .await
            .unwrap();
        socket
            .send(ws::Message::Text("first".into()))
            .await
            .unwrap();
        let hooks = FrameHooks::default()
            .on_client_text(|text| {
                if text.starts_with("refuse") {
                    ClientText::Answer(format!("refused: {text}"))
                } else {
                    ClientText::Forward
                }
            })
            .on_upstream_text(|text| text.starts_with("hello").then(|| text.to_uppercase()));
        socket.relay(client, hooks).await;
    })
}

#[tokio::test]
async fn websocket_hooks_answer_client_texts_and_rewrite_upstream_texts() {
    let upstream_addr = serve(Router::new().route("/ws", get(echo_ws))).await;
    let upstream_url: Url = format!("ws://{upstream_addr}/ws").parse().unwrap();
    let relay_addr = serve(
        Router::new()
            .route("/ws", get(relay_ws_with_hooks))
            .with_state(upstream_url),
    )
    .await;
    let (mut client, _) = connect_async(format!("ws://{relay_addr}/ws"))
        .await
        .unwrap();

    assert_eq!(
        next(&mut client).await,
        UpstreamMessage::Text("HELLO NONE".into())
    );
    assert_eq!(
        next(&mut client).await,
        UpstreamMessage::Text("first".into()),
        "the message sent before the relay reaches upstream first"
    );
    for (sent, expected) in [
        ("refuse this", "refused: refuse this"),
        ("pass this", "pass this"),
        ("hello again", "HELLO AGAIN"),
    ] {
        client
            .send(UpstreamMessage::Text(sent.into()))
            .await
            .unwrap();
        assert_eq!(
            next(&mut client).await,
            UpstreamMessage::Text(expected.into())
        );
    }
    client
        .send(UpstreamMessage::Binary(Bytes::from_static(b"refuse")))
        .await
        .unwrap();
    client
        .send(UpstreamMessage::Text("after binary".into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut client).await,
        UpstreamMessage::Text("after binary".into()),
        "with a client hook, binary messages never reach upstream"
    );
}

#[tokio::test]
async fn websocket_accepts_subprotocol_from_a_later_offer_line() {
    let addr = serve(Router::new().route("/ws", get(echo_ws))).await;
    let url = format!("ws://{addr}/ws").parse().unwrap();
    let offers = map(&[
        ("sec-websocket-protocol", "unused"),
        ("sec-websocket-protocol", "mesh"),
    ]);
    let socket = UpstreamSocket::connect(url, offers).await.unwrap();
    assert_eq!(socket.response_headers()["sec-websocket-protocol"], "mesh");
}

#[tokio::test]
async fn websocket_bridge_acknowledges_upstream_close_and_forwards_its_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("ws://{}/ws", listener.local_addr().unwrap())
        .parse::<Url>()
        .unwrap();
    let close = UpstreamMessage::Close(Some(tungstenite::protocol::CloseFrame {
        code: tungstenite::protocol::frame::coding::CloseCode::Away,
        reason: "upstream going away".into(),
    }));
    let expected = close.clone();
    let upstream = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        socket.send(close.clone()).await.unwrap();
        assert_eq!(socket.next().await.unwrap().unwrap(), close);
    });
    let relay_addr = serve(
        Router::new()
            .route("/ws", get(relay_ws))
            .with_state(upstream_url),
    )
    .await;
    let (mut client, _) = connect_async(format!("ws://{relay_addr}/ws"))
        .await
        .unwrap();
    assert_eq!(next(&mut client).await, expected);
    client.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), upstream)
        .await
        .expect("upstream did not get a close reply")
        .unwrap();
}

#[tokio::test]
async fn websocket_handshake_refusal_is_rejected() {
    async fn refuse() -> Response {
        let mut response = (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
        response
            .headers_mut()
            .insert("x-mesh-denied", HeaderValue::from_static("reserve"));
        response
    }
    let upstream_addr = serve(Router::new().route("/ws", get(refuse))).await;
    let upstream_url: Url = format!("ws://{upstream_addr}/ws").parse().unwrap();

    let err = UpstreamSocket::connect(upstream_url.clone(), HeaderMap::new())
        .await
        .err()
        .expect("handshake must fail");
    match &err {
        mesh_relay::Error::Rejected {
            status,
            headers,
            body,
        } => {
            assert_eq!(*status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(headers["x-mesh-denied"], "reserve");
            assert_eq!(body, "slow down");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }

    let relay_addr = serve(
        Router::new()
            .route("/ws", get(relay_ws))
            .with_state(upstream_url),
    )
    .await;
    let Err(tungstenite::Error::Http(response)) =
        connect_async(format!("ws://{relay_addr}/ws")).await
    else {
        panic!("expected an HTTP refusal");
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["x-mesh-denied"], "reserve");
    assert_eq!(response.body().as_deref(), Some(b"slow down".as_slice()));
}

#[tokio::test]
async fn other_errors_become_bad_gateway() {
    let response = mesh_relay::Error::WebSocket("boom".into()).into_response();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(body, "WebSocket error: boom");
}
