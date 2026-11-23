//! Consumer WebSockets routed by the model of their first message.

use std::sync::Mutex;

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use mesh_core::protocol::{DENIED_MODEL, MODEL_HEADER};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use mesh_core::api::{TokenUsage, Transport};

use super::logs::completed;
use super::models::{codex, logged};
use super::*;

/// The accounts whose relay route the fake daemon was asked to open, each
/// with the model that the server named.
type Opened = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// How the fake daemon answers the handshake of one account.
#[derive(Clone, Copy)]
enum Answer {
    /// `429` with this `x-mesh-denied` value.
    Denied(&'static str),
    /// An upstream `429` that asks for a rest of 30 seconds.
    Limited,
    /// Any other refusal.
    Status(StatusCode),
}

/// The metadata event that the fake daemon sends after the first message.
const METADATA: &str = r#"{"type":"codex.response.metadata","headers":{"x-models-etag":"W/\"upstream\"","x-codex-turn-state":"turn"}}"#;

/// A daemon that answers the handshakes of the listed accounts, and opens
/// the sockets of all others. An open socket answers its first message with
/// [`METADATA`], then echoes each text message as `<account>: <text>`.
fn socket_daemon(answers: &[(&'static str, Answer)]) -> (Router, Opened) {
    let answers: Arc<Vec<_>> = Arc::new(answers.to_vec());
    let opened = Opened::default();
    let seen = opened.clone();
    let handle = move |Path((id, _)): Path<(String, String)>,
                       headers: HeaderMap,
                       upgrade: WebSocketUpgrade| {
        let answers = answers.clone();
        let seen = seen.clone();
        async move {
            let model = headers
                .get(MODEL_HEADER)
                .map(|value| value.to_str().unwrap().to_owned());
            seen.lock().unwrap().push((id.clone(), model));
            match answers.iter().find(|(account, _)| *account == id) {
                Some((_, Answer::Denied(reason))) => {
                    (StatusCode::TOO_MANY_REQUESTS, [(DENIED_HEADER, *reason)]).into_response()
                }
                Some((_, Answer::Limited)) => (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "30")],
                    r#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#,
                )
                    .into_response(),
                Some((_, Answer::Status(status))) => (*status, "refused").into_response(),
                None => upgrade.on_upgrade(move |socket| echo(id, socket)),
            }
        }
    };
    let router = Router::new().route("/relay/{id}/{*path}", any(handle));
    (router, opened)
}

async fn echo(id: String, mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message as Daemon;

    let mut first = true;
    while let Some(Ok(message)) = socket.recv().await {
        let Daemon::Text(text) = message else {
            if matches!(message, Daemon::Close(_)) {
                break;
            }
            continue;
        };
        if std::mem::take(&mut first) {
            let _ = socket.send(Daemon::Text(METADATA.into())).await;
        }
        let echoed = format!("{id}: {}", text.as_str());
        if socket.send(Daemon::Text(echoed.into())).await.is_err() {
            break;
        }
    }
}

fn opened(opened: &Opened) -> Vec<(String, Option<String>)> {
    opened.lock().unwrap().clone()
}

type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: SocketAddr) -> Client {
    let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/codex/responses"))
        .await
        .unwrap();
    client
}

fn create(model: &str) -> String {
    json!({"type": "response.create", "model": model, "input": []}).to_string()
}

async fn next_text(client: &mut Client) -> String {
    let message = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("a message arrives")
        .unwrap()
        .unwrap();
    message.into_text().unwrap().as_str().to_owned()
}

/// Send `first`, and return the `error` event that ends the socket.
async fn refused(addr: SocketAddr, first: &str) -> Value {
    let mut client = connect(addr).await;
    client.send(Message::Text(first.into())).await.unwrap();
    let event: Value = serde_json::from_str(&next_text(&mut client).await).unwrap();
    assert_eq!(event["type"], "error");
    let end = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("the socket ends");
    assert!(matches!(end, None | Some(Ok(Message::Close(_)))), "{end:?}");
    event
}

async fn socket_state(
    accounts: Vec<AccountReport>,
    answers: &[(&'static str, Answer)],
) -> (Arc<AppState>, Opened, SocketAddr) {
    let (router, opened) = socket_daemon(answers);
    let state = relay_state(router, accounts).await;
    let addr = serve(state.clone()).await;
    (state, opened, addr)
}

#[tokio::test]
async fn a_websocket_goes_to_an_account_that_allows_the_model_of_its_first_message() {
    let (state, sockets, addr) = socket_state(
        vec![codex("a", Some(&["gpt-a*"])), codex("b", Some(&["gpt-b*"]))],
        &[],
    )
    .await;
    let mut client = connect(addr).await;
    assert!(
        opened(&sockets).is_empty(),
        "no account before the first message"
    );
    assert_eq!(in_flight(&state, "a") + in_flight(&state, "b"), 0);

    let first = create("gpt-b1");
    client
        .send(Message::Text(first.clone().into()))
        .await
        .unwrap();
    let metadata: Value = serde_json::from_str(&next_text(&mut client).await).unwrap();
    assert_eq!(
        metadata,
        json!({"type": "codex.response.metadata", "headers": {"x-codex-turn-state": "turn"}}),
        "the upstream models etag never reaches Codex"
    );
    assert_eq!(next_text(&mut client).await, format!("b: {first}"));
    assert_eq!(
        opened(&sockets),
        [("b".to_owned(), Some("gpt-b1".to_owned()))]
    );
    assert_eq!(in_flight(&state, "b"), 1);

    let later = create("gpt-a1");
    client
        .send(Message::Text(later.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        next_text(&mut client).await,
        format!("b: {later}"),
        "a socket keeps its account; the daemon checks later models"
    );

    client.close(None).await.unwrap();
    while client.next().await.is_some() {}
    wait_for_in_flight(&state, "b", 0).await;
    let records = logged(&state, 1).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 101);
    assert_eq!(records[0].account_id.as_ref().unwrap().as_str(), "b");
}

#[tokio::test]
async fn a_websocket_for_a_model_that_no_account_allows_gets_model_not_found() {
    let (state, sockets, addr) = socket_state(vec![codex("a", Some(&["gpt-a*"]))], &[]).await;
    let event = refused(addr, &create("other")).await;
    assert_eq!(event["status"], 404);
    assert_eq!(event["error"]["code"], "model_not_found");
    assert_eq!(event["error"]["type"], "invalid_request_error");
    assert!(opened(&sockets).is_empty());
    let records = logged(&state, 1).await;
    assert_eq!(records[0].status, 404);
    assert_eq!(records[0].account_id, None);
}

#[tokio::test]
async fn a_websocket_tries_the_next_account_until_one_opens() {
    let (state, sockets, addr) = socket_state(
        vec![account("a", 0.9), account("b", 0.8), account("c", 0.7)],
        &[
            ("a", Answer::Denied(DENIED_MODEL)),
            ("b", Answer::Denied("reserve")),
        ],
    )
    .await;
    let mut client = connect(addr).await;
    let first = create("gpt-6");
    client
        .send(Message::Text(first.clone().into()))
        .await
        .unwrap();
    next_text(&mut client).await;
    assert_eq!(
        next_text(&mut client).await,
        format!("c: {first}"),
        "only the account that opened gets the first message"
    );
    let model = Some("gpt-6".to_owned());
    assert_eq!(
        opened(&sockets),
        [
            ("a".to_owned(), model.clone()),
            ("b".to_owned(), model.clone()),
            ("c".to_owned(), model),
        ]
    );
    let records = logged(&state, 1).await;
    assert_eq!((records[0].status, records[0].attempts), (101, 3));
    let cooling = |id: &str| {
        state
            .with_registry(|r| r.view(&id.parse().unwrap(), Utc::now()))
            .unwrap()
            .cooldown_until
            .is_some()
    };
    assert!(!cooling("a"), "a model refusal rests nothing");
    assert!(cooling("b"));
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn every_account_refusing_the_model_ends_in_model_not_found() {
    let (_state, sockets, addr) = socket_state(
        vec![account("a", 0.9), account("b", 0.8)],
        &[
            ("a", Answer::Denied(DENIED_MODEL)),
            ("b", Answer::Denied(DENIED_MODEL)),
        ],
    )
    .await;
    let event = refused(addr, &create("gpt-6")).await;
    assert_eq!(event["status"], 404);
    assert_eq!(event["error"]["code"], "model_not_found");
    assert_eq!(opened(&sockets).len(), 2);
}

#[tokio::test]
async fn the_last_upstream_429_is_the_answer_when_no_account_opens() {
    let (_state, _sockets, addr) = socket_state(
        vec![account("a", 0.9), account("b", 0.8)],
        &[("a", Answer::Limited), ("b", Answer::Denied("reserve"))],
    )
    .await;
    let event = refused(addr, &create("gpt-6")).await;
    assert_eq!(event["status"], 429);
    assert_eq!(event["error"]["type"], "usage_limit_reached");
    assert_eq!(event["headers"]["retry-after"], "30");
    assert!(event["headers"].get(DENIED_HEADER).is_none());
}

#[tokio::test]
async fn another_refusal_ends_the_socket_with_its_status() {
    let (_state, sockets, addr) = socket_state(
        vec![account("a", 0.9), account("b", 0.8), account("c", 0.7)],
        &[
            ("a", Answer::Denied("reserve")),
            ("b", Answer::Status(StatusCode::INTERNAL_SERVER_ERROR)),
        ],
    )
    .await;
    let event = refused(addr, &create("gpt-6")).await;
    assert_eq!(event["status"], 500);
    assert_eq!(event["error"]["message"], "refused");
    let tried: Vec<String> = opened(&sockets).into_iter().map(|(id, _)| id).collect();
    assert_eq!(tried, ["a", "b"]);
}

#[tokio::test]
async fn no_account_with_headroom_is_a_503_event() {
    let state = state();
    let addr = serve(state).await;
    let event = refused(addr, &create("gpt-6")).await;
    assert_eq!(event["status"], 503);
    assert_eq!(event["error"]["type"], "mesh_unavailable");
}

#[tokio::test]
async fn a_client_that_closes_before_its_first_message_uses_no_account() {
    let (state, sockets, addr) = socket_state(vec![account("a", 0.9)], &[]).await;
    let mut client = connect(addr).await;
    client.close(None).await.unwrap();
    while client.next().await.is_some() {}
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(opened(&sockets).is_empty());
    assert_eq!(in_flight(&state, "a"), 0);
    let records = state
        .db
        .run(|db| db.recent_requests_of_anyone(5))
        .await
        .unwrap();
    assert!(records.is_empty(), "{records:?}");
}

const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2}}}}"#;

/// A daemon whose sockets answer each text message with a delta and a
/// completed response.
fn completing_daemon() -> Router {
    use axum::extract::ws::Message as Daemon;

    let handle = |upgrade: WebSocketUpgrade| async move {
        upgrade.on_upgrade(|mut socket| async move {
            while let Some(Ok(message)) = socket.recv().await {
                if !matches!(message, Daemon::Text(_)) {
                    continue;
                }
                let delta = r#"{"type":"response.output_text.delta","delta":"hi"}"#;
                for frame in [delta, COMPLETED] {
                    if socket.send(Daemon::Text(frame.into())).await.is_err() {
                        return;
                    }
                }
            }
        })
    };
    Router::new().route("/relay/{id}/{*path}", any(handle))
}

#[tokio::test]
async fn a_websocket_is_logged_when_it_opens_and_again_when_it_closes() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let state = relay_state(completing_daemon(), vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let mut request = format!("ws://{addr}/codex/responses?model=x")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("user-agent", "codex-cli/0.9".parse().unwrap());
    request
        .headers_mut()
        .insert("session_id", "sess-ws".parse().unwrap());
    let (mut client, _) = tokio_tungstenite::connect_async(request).await.unwrap();

    for _ in 0..2 {
        client
            .send(Message::Text(create("gpt-5").into()))
            .await
            .unwrap();
        next_text(&mut client).await;
        assert!(next_text(&mut client).await.contains("response.completed"));
    }
    let open = logged(&state, 1).await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].status, 101);
    assert_eq!(open[0].duration_ms, None, "the socket is still open");
    assert_eq!((open[0].responses, open[0].usage), (0, None));

    client.close(None).await.unwrap();
    while client.next().await.is_some() {}
    let entries = completed(&state, 1).await;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.id, open[0].id);
    assert_eq!(entry.transport, Transport::Websocket);
    assert_eq!(
        (entry.method.as_str(), entry.path.as_str()),
        ("GET", "responses")
    );
    assert_eq!(entry.model.as_deref(), Some("gpt-5"));
    assert_eq!(entry.client.as_deref(), Some("codex-cli/0.9"));
    assert_eq!(entry.session.as_deref(), Some("sess-ws"));
    assert_eq!(entry.node.as_deref(), Some("node"));
    assert_eq!(entry.error, None);
    assert_eq!(entry.responses, 2);
    assert_eq!(
        entry.usage,
        Some(TokenUsage {
            input: 20,
            cache_read: 8,
            cache_write: 0,
            output: 10,
            reasoning: 4,
        })
    );
    assert!(entry.duration_ms.unwrap() >= entry.latency_ms);
}

#[tokio::test]
async fn a_websocket_error_frame_is_logged_as_the_error() {
    use axum::extract::ws::Message as Daemon;

    let handle = |upgrade: WebSocketUpgrade| async move {
        upgrade.on_upgrade(|mut socket| async move {
            while let Some(Ok(Daemon::Text(_))) = socket.recv().await {
                let frame = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#;
                let _ = socket.send(Daemon::Text(frame.into())).await;
            }
        })
    };
    let router = Router::new().route("/relay/{id}/{*path}", any(handle));
    let state = relay_state(router, vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let mut client = connect(addr).await;
    client
        .send(Message::Text(create("gpt-5").into()))
        .await
        .unwrap();
    next_text(&mut client).await;
    client.close(None).await.unwrap();
    while client.next().await.is_some() {}
    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].status, 101);
    assert_eq!(
        entries[0].error.as_deref(),
        Some("The usage limit has been reached")
    );
    assert_eq!(entries[0].responses, 0);
}

#[tokio::test]
async fn a_refused_websocket_logs_the_refusal() {
    let (state, _sockets, addr) =
        socket_state(vec![account("a", 0.9)], &[("a", Answer::Limited)]).await;
    refused(addr, &create("gpt-6")).await;
    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].status, 429);
    assert_eq!(entries[0].error.as_deref(), Some("limit"));
    assert_eq!(entries[0].transport, Transport::Websocket);
}
