//! What the request log records of relayed requests: their details, the
//! tokens their responses used, and the errors they ended in.

use mesh_core::api::{LogEntry, TokenUsage, Transport};

use super::*;

const SSE: &str = "text/event-stream; charset=utf-8";

pub(super) async fn completed(state: &AppState, count: usize) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    for _ in 0..100 {
        entries = state
            .db
            .run(|db| db.recent_requests_of_anyone(20))
            .await
            .unwrap();
        if entries.len() >= count && entries.iter().all(|entry| entry.duration_ms.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    entries
}

fn claude_account(id: &str) -> AccountReport {
    AccountReport {
        provider: Provider::Claude,
        ..account(id, 0.9)
    }
}

fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64, reasoning: u64) -> TokenUsage {
    TokenUsage {
        input,
        cache_read,
        cache_write,
        output,
        reasoning,
    }
}

async fn claude_request(addr: SocketAddr, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}/claude/v1/messages"))
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
}

const CODEX_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"usage\":null}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":100,",
    "\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":20,",
    "\"output_tokens_details\":{\"reasoning_tokens\":5},\"total_tokens\":120}}}\n\n",
);

#[tokio::test]
async fn a_codex_stream_is_logged_with_its_details_and_tokens() {
    let stub = relays(vec![(
        "a",
        Reply::status(200)
            .header("content-type", SSE)
            .body(CODEX_SSE),
    )]);
    let state = relay_state(stub.router(), vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let response = codex_request(addr)
        .query(&[("stream", "true")])
        .header("user-agent", "codex-cli/0.9 (linux)")
        .header("session_id", "sess-1")
        .body(r#"{"model":"gpt-5","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), CODEX_SSE);

    let entries = completed(&state, 1).await;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.provider, Provider::Codex);
    assert_eq!(entry.transport, Transport::Http);
    assert_eq!(entry.method, "POST");
    assert_eq!(entry.path, "responses");
    assert_eq!(entry.model.as_deref(), Some("gpt-5"));
    assert_eq!(entry.client.as_deref(), Some("codex-cli/0.9 (linux)"));
    assert_eq!(entry.session.as_deref(), Some("sess-1"));
    assert_eq!(entry.account_id.as_ref().unwrap().as_str(), "a");
    assert_eq!(entry.node.as_deref(), Some("node"));
    assert_eq!((entry.status, entry.attempts), (200, 1));
    assert_eq!(entry.error, None);
    assert_eq!(entry.responses, 1);
    assert_eq!(entry.usage, Some(usage(100, 40, 0, 20, 5)));
    assert!(entry.duration_ms.unwrap() >= entry.latency_ms);
}

#[tokio::test]
async fn a_codex_stream_without_a_content_type_is_logged_with_its_tokens() {
    let stub = relays(vec![("a", Reply::status(200).body(CODEX_SSE))]);
    let state = relay_state(stub.router(), vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let response = send_codex(addr, r#"{"model":"gpt-5","input":[]}"#).await;
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("content-type").is_none());
    assert_eq!(response.text().await.unwrap(), CODEX_SSE);

    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].responses, 1);
    assert_eq!(entries[0].usage, Some(usage(100, 40, 0, 20, 5)));
    assert_eq!(entries[0].transport, Transport::Http);
}

#[tokio::test]
async fn a_json_answer_without_a_content_type_is_logged_with_its_tokens() {
    let answer = r#"{"id":"r","usage":{"input_tokens":8,"output_tokens":2}}"#;
    let stub = relays(vec![("a", Reply::status(200).body(answer))]);
    let state = relay_state(stub.router(), vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let response = send_codex(addr, "{}").await;
    assert_eq!(response.text().await.unwrap(), answer);

    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].responses, 1);
    assert_eq!(entries[0].usage, Some(usage(8, 0, 0, 2, 0)));
}

#[tokio::test]
async fn the_owner_of_the_serving_daemon_is_logged_and_outlives_the_account() {
    const ALICE: &str = "alice@example.com";
    let stub = relays(vec![("a", Reply::status(200).body("served"))]);
    let state = state();
    FakeDaemon::connect(&state, "node", stub.router()).await;
    apply(&state, report("node", vec![account("a", 0.9)]), Some(ALICE)).await;
    let addr = serve_with_identity(state.clone()).await;
    let response = codex_request(addr)
        .header(LOGIN_HEADER, "bob@example.com")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "served");

    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].login.as_deref(), Some("bob@example.com"));
    assert_eq!(entries[0].owner.as_deref(), Some(ALICE));

    apply(&state, report("node", vec![]), Some(ALICE)).await;
    let inbound = state
        .db
        .run(|db| {
            db.recent_requests(
                &crate::db::LogOwner::Login(ALICE.to_owned()),
                Some(mesh_core::api::Direction::Inbound),
                10,
                None,
            )
        })
        .await
        .unwrap();
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].owner.as_deref(), Some(ALICE));
    assert_eq!(inbound[0].account_id.as_ref().unwrap().as_str(), "a");
}

#[tokio::test]
async fn a_late_response_keeps_the_owner_at_dispatch() {
    use axum::response::IntoResponse;
    use tokio::sync::Notify;

    let arrived = std::sync::Arc::new(Notify::new());
    let release = std::sync::Arc::new(Notify::new());
    let route = {
        let arrived = arrived.clone();
        let release = release.clone();
        axum::routing::post(move || async move {
            arrived.notify_one();
            release.notified().await;
            "served".into_response()
        })
    };
    let state = state();
    let daemon = FakeDaemon::connect(
        &state,
        "node",
        axum::Router::new().route("/relay/a/responses", route),
    )
    .await;
    apply(
        &state,
        report("node", vec![account("a", 0.9)]),
        Some("alice@example.com"),
    )
    .await;
    let addr = serve_with_identity(state.clone()).await;
    let response =
        tokio::spawn(async move { codex_request(addr).body("{}").send().await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
        .await
        .unwrap();
    state.with_registry(|registry| {
        assert!(registry.detach_tunnel("node", daemon.generation));
        registry.apply_report(
            report("node", vec![account("a", 0.9)]),
            Some("bob@example.com".to_owned()),
            chrono::Utc::now(),
        );
    });
    release.notify_one();
    assert_eq!(response.await.unwrap().text().await.unwrap(), "served");
    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].owner.as_deref(), Some("alice@example.com"));
}

#[tokio::test]
async fn a_request_that_no_account_served_has_no_owner() {
    let state = state();
    let addr = serve(state.clone()).await;
    send_codex(addr, "{}").await.bytes().await.unwrap();
    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].owner, None);
}

const CLAUDE_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":10,",
    "\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn a_claude_stream_is_logged_with_its_tokens() {
    let stub = Stub::new();
    stub.on(
        Method::POST,
        "/relay/c/v1/messages",
        Reply::status(200)
            .header("content-type", SSE)
            .body(CLAUDE_SSE),
    );
    let state = relay_state(stub.router(), vec![claude_account("c")]).await;
    let addr = serve(state.clone()).await;
    let response = claude_request(addr, r#"{"model":"claude-opus-4","messages":[]}"#).await;
    assert_eq!(response.text().await.unwrap(), CLAUDE_SSE);

    let entries = completed(&state, 1).await;
    let entry = &entries[0];
    assert_eq!(entry.provider, Provider::Claude);
    assert_eq!(entry.path, "v1/messages");
    assert_eq!(entry.model.as_deref(), Some("claude-opus-4"));
    assert_eq!(entry.responses, 1);
    assert_eq!(entry.usage, Some(usage(60, 30, 20, 15, 0)));
}

#[tokio::test]
async fn a_claude_json_answer_is_logged_with_its_tokens() {
    let stub = Stub::new();
    let answer = serde_json::json!({
        "id": "m",
        "type": "message",
        "content": [{"type": "text", "text": "say \"usage\" now"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 3,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 7,
            "output_tokens": 4,
        },
    });
    stub.on(
        Method::POST,
        "/relay/c/v1/messages",
        Reply::json(200, &answer),
    );
    let state = relay_state(stub.router(), vec![claude_account("c")]).await;
    let addr = serve(state.clone()).await;
    let response = claude_request(addr, r#"{"model":"claude-opus-4"}"#).await;
    assert_eq!(response.json::<serde_json::Value>().await.unwrap(), answer);

    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].responses, 1);
    assert_eq!(entries[0].usage, Some(usage(10, 7, 0, 4, 0)));
}

#[tokio::test]
async fn unsupported_endpoints_are_rejected_before_they_can_be_counted() {
    let stub = Stub::new();
    let state = relay_state(stub.router(), vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/codex/v1/realtime"))
        .json(&serde_json::json!({"model":"gpt-5"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(completed(&state, 1).await.is_empty());
}

#[tokio::test]
async fn an_upstream_error_message_is_logged() {
    let stub = Stub::new();
    stub.on(
        Method::POST,
        "/relay/c/v1/messages",
        Reply::json(
            400,
            &serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "max_tokens is too large"},
            }),
        ),
    );
    stub.on(
        Method::POST,
        "/relay/a/responses",
        Reply::status(500).body("upstream exploded\n"),
    );
    let state = relay_state(stub.router(), vec![claude_account("c"), account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    let response = claude_request(addr, "{}").await;
    assert_eq!(response.status(), 400);
    response.bytes().await.unwrap();
    let response = send_codex(addr, "{}").await;
    assert_eq!(response.status(), 500);
    response.bytes().await.unwrap();

    let entries = completed(&state, 2).await;
    let of = |provider| entries.iter().find(|e| e.provider == provider).unwrap();
    let claude = of(Provider::Claude);
    assert_eq!(claude.status, 400);
    assert_eq!(claude.error.as_deref(), Some("max_tokens is too large"));
    assert_eq!((claude.responses, claude.usage), (0, None));
    let codex = of(Provider::Codex);
    assert_eq!(codex.status, 500);
    assert_eq!(codex.error.as_deref(), Some("upstream exploded"));
}

#[tokio::test]
async fn a_long_upstream_error_is_clipped() {
    let long = "e".repeat(5000);
    let stub = relays(vec![(
        "a",
        Reply::json(400, &serde_json::json!({"error": {"message": long}})),
    )]);
    let state = relay_state(stub.router(), vec![account("a", 0.9)]).await;
    let addr = serve(state.clone()).await;
    send_codex(addr, "{}").await.bytes().await.unwrap();
    let entries = completed(&state, 1).await;
    let error = entries[0].error.as_deref().unwrap();
    assert_eq!(error.chars().count(), 300);
    assert!(error.ends_with('…'));
}

#[tokio::test]
async fn the_messages_mesh_makes_are_logged() {
    let state = state();
    let addr = serve(state.clone()).await;
    let response = send_codex(addr, "{}").await;
    assert_eq!(response.status(), 503);
    response.bytes().await.unwrap();

    let mut restricted = account("a", 0.9);
    restricted.models = Some(mesh_core::ModelAllowlist::new(["gpt-a*"]).unwrap());
    let stub = relays(vec![("a", Reply::status(200))]);
    let narrow = relay_state(stub.router(), vec![restricted]).await;
    let narrow_addr = serve(narrow.clone()).await;
    let response = send_codex(narrow_addr, r#"{"model":"other"}"#).await;
    assert_eq!(response.status(), 404);
    response.bytes().await.unwrap();

    let entries = completed(&state, 1).await;
    assert_eq!(entries[0].status, 503);
    assert_eq!(
        entries[0].error.as_deref(),
        Some("mesh: no account has headroom")
    );
    assert_eq!(entries[0].account_id, None);
    let entries = completed(&narrow, 1).await;
    assert_eq!(entries[0].status, 404);
    assert_eq!(entries[0].model.as_deref(), Some("other"));
    assert_eq!(
        entries[0].error.as_deref(),
        Some("mesh: no account may serve the model other")
    );
}
