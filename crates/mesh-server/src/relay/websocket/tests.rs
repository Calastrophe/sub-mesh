use std::net::SocketAddr;

use axum::Router;
use axum::http::Method;
use axum::routing::get;
use futures_util::SinkExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

use super::*;
use crate::testing::serve_app;

#[test]
fn the_models_etag_leaves_a_metadata_event_and_nothing_else_changes() {
    let event = r#"{"type":"codex.response.metadata","headers":{"x-models-etag":"W/\"abc\"","x-codex-turn-state":"state"}}"#;
    let stripped: Value = serde_json::from_str(&without_models_etag(event).unwrap()).unwrap();
    assert_eq!(
        stripped,
        json!({
            "type": "codex.response.metadata",
            "headers": {"x-codex-turn-state": "state"},
        })
    );

    for kept in [
        r#"{"type":"codex.response.metadata","headers":{"x-codex-turn-state":"state"}}"#,
        r#"{"type":"response.output_text.delta","delta":"{\"type\":\"codex.response.metadata\"}"}"#,
        r#"{"type":"codex.response.metadata","headers":"x-models-etag"}"#,
        r#"{"type":"codex.response.metadata""#,
        "plain text",
    ] {
        assert_eq!(without_models_etag(kept), None, "{kept}");
    }
}

#[test]
fn the_model_of_the_first_message_goes_to_the_daemon_as_a_header() {
    let mut client_headers = HeaderMap::new();
    client_headers.insert("session-id", HeaderValue::from_static("s"));
    let job = Job {
        method: Method::GET,
        headers: client_headers,
        rest: "/responses",
        query: None,
    };
    let headers = handshake_headers(&job, Some("gpt-6"));
    assert_eq!(headers[MODEL_HEADER], "gpt-6");
    assert_eq!(headers["session-id"], "s");
    assert!(!handshake_headers(&job, None).contains_key(MODEL_HEADER));
    assert!(
        !handshake_headers(&job, Some("bad\nmodel")).contains_key(MODEL_HEADER),
        "a model that cannot be a header value is left to the daemon's message check"
    );
}

#[tokio::test]
async fn a_refusal_becomes_the_error_event_codex_reads() {
    let refusal = (
        StatusCode::TOO_MANY_REQUESTS,
        [("retry-after", "7"), ("content-type", "application/json")],
        r#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#,
    )
        .into_response();
    let event: Value = serde_json::from_str(&error_event(refusal).await).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["status"], 429);
    assert_eq!(
        event["error"],
        json!({"type": "usage_limit_reached", "message": "limit"})
    );
    assert_eq!(event["headers"]["retry-after"], "7");

    let refusal = (StatusCode::BAD_GATEWAY, "tunnel broke\n").into_response();
    let event: Value = serde_json::from_str(&error_event(refusal).await).unwrap();
    assert_eq!(event["status"], 502);
    assert_eq!(event["error"], json!({"message": "tunnel broke"}));
}

/// Serve one WebSocket whose first message is read within `limit`, and
/// report what was read.
async fn first_message_server(
    limit: Duration,
) -> (SocketAddr, mpsc::Receiver<Option<ws::Message>>) {
    let (tx, rx) = mpsc::channel(1);
    let app = Router::new().route(
        "/ws",
        get(move |upgrade: WebSocketUpgrade| {
            let tx = tx.clone();
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    let first = first_message(&mut socket, limit).await;
                    tx.send(first).await.unwrap();
                })
            }
        }),
    );
    (serve_app(app).await, rx)
}

#[tokio::test]
async fn the_first_message_skips_pings_and_ends_on_close_or_timeout() {
    let (addr, mut read) = first_message_server(Duration::from_secs(5)).await;
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    client.send(ClientMessage::Ping("p".into())).await.unwrap();
    client
        .send(ClientMessage::Text("first".into()))
        .await
        .unwrap();
    assert_eq!(
        read.recv().await.unwrap(),
        Some(ws::Message::Text("first".into()))
    );

    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    client.close(None).await.unwrap();
    assert_eq!(read.recv().await.unwrap(), None);

    let (addr, mut read) = first_message_server(Duration::from_millis(50)).await;
    let (_client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    assert_eq!(read.recv().await.unwrap(), None);
}
