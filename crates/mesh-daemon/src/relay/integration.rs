use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderName;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use mesh_core::ModelAllowlist;
use mesh_core::protocol::{DENIED_HEADER, DENIED_MODEL, HEADROOM_HEADER, MODEL_HEADER};
use mesh_providers::TokenStore;
use mesh_relay::FrameHooks;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::state::AppState;
use crate::tunnel::testing::{Reply, TestServer, Tunnel, WAIT, daemon_state};

struct Relay {
    state: Arc<AppState>,
    store: TokenStore,
    tunnel: Tunnel,
}

impl Relay {
    async fn start(name: &str, upstream: &MockServer) -> Self {
        let base = format!("{}/backend-api", upstream.uri());
        Self::start_at(name, upstream, &base).await
    }

    /// Start a daemon whose Codex requests go to `upstream_base`, while
    /// token refreshes and usage polls go to `mock`.
    async fn start_at(name: &str, mock: &MockServer, upstream_base: &str) -> Self {
        let base = mock.uri();
        let server = TestServer::start().await;
        let state = daemon_state(
            name,
            &server.url,
            "",
            &format!(
                r#"
                [endpoints.codex]
                upstream_base = "{upstream_base}"
                token_url = "{base}/tokens"
                usage_url = "{base}/usage"
                "#
            ),
            true,
        );
        let (tunnel, store) = Tunnel::with_codex_account(&state, server).await;
        Self {
            state,
            store,
            tunnel,
        }
    }

    async fn request(&self, path: &str) -> Reply {
        self.tunnel
            .request(Method::POST, &format!("/relay/codex-1/{path}"))
            .header("authorization", "Bearer consumer")
            .header("x-client-marker", "passed")
            .header("cookie", "consumer=secret")
            .body("request body")
            .send()
            .await
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state.settings.data_dir);
    }
}

async fn mock_usage(upstream: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn forwards_headers_learns_windows_and_enforces_reserve() {
    let upstream = MockServer::start().await;
    mock_usage(&upstream).await;
    for (request_path, used) in [("low", "25"), ("high", "80")] {
        Mock::given(method("POST"))
            .and(path(format!("/backend-api/{request_path}")))
            .and(query_param("include", "usage"))
            .and(header("authorization", "Bearer access"))
            .and(header("x-client-marker", "passed"))
            .and(|request: &wiremock::Request| !request.headers.contains_key("cookie"))
            .and(wiremock::matchers::body_string("request body"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-codex-primary-used-percent", used)
                    .insert_header("x-upstream-marker", "passed")
                    .insert_header("set-cookie", "upstream=secret")
                    .set_body_string("response body"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
    }
    let relay = Relay::start("relay-reserve", &upstream).await;
    let response = relay.request("low?include=usage").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-upstream-marker"], "passed");
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(response.headers()[HEADROOM_HEADER], "0.4500");
    assert_eq!(response.text().await, "response body");
    let report = relay.state.daemon_report();
    assert_eq!(report.accounts[0].windows[0].id, "primary");
    assert_eq!(report.accounts[0].windows[0].used, 0.25);

    let response = relay.request("high?include=usage").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[HEADROOM_HEADER], "deny");
    let response = relay.request("low?include=usage").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[DENIED_HEADER], "reserve");
    relay.state.stop_background().await;
    upstream.verify().await;
}

#[tokio::test]
async fn refuses_only_the_requests_for_models_the_account_does_not_allow() {
    let upstream = MockServer::start().await;
    mock_usage(&upstream).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/ok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&upstream)
        .await;
    let relay = Relay::start("relay-models", &upstream).await;
    let id = "codex-1".parse().unwrap();
    relay
        .state
        .set_account_models(&id, Some(ModelAllowlist::new(["gpt-*", "o3"]).unwrap()))
        .unwrap();
    let send = |body: &'static str| {
        relay
            .tunnel
            .request(Method::POST, "/relay/codex-1/ok")
            .body(body)
            .send()
    };

    assert_eq!(
        send(r#"{"model": "GPT-6", "input": []}"#).await.status(),
        StatusCode::OK
    );
    for unreadable in [r#"{"input": [{"model": "claude-opus-5-5"}]}"#, "not json"] {
        assert_eq!(send(unreadable).await.status(), StatusCode::BAD_REQUEST);
    }
    let encoded = relay
        .tunnel
        .request(Method::POST, "/relay/codex-1/ok")
        .header("content-encoding", "gzip")
        .body("opaque")
        .send()
        .await;
    assert_eq!(encoded.status(), StatusCode::BAD_REQUEST);

    let response = send(r#"{"model": "claude-opus-5-5"}"#).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[DENIED_HEADER], DENIED_MODEL);
    let body: serde_json::Value = response.json().await;
    assert_eq!(body["model"], "claude-opus-5-5");

    let repeated = send(r#"{"model": "gpt-6", "model": "claude-opus-5-5"}"#).await;
    assert_eq!(
        repeated.status(),
        StatusCode::BAD_REQUEST,
        "a repeated model key cannot slip past the allowlist"
    );

    let report = relay.state.daemon_report();
    assert!(report.accounts[0].admission.is_admit());
    assert_eq!(
        report.accounts[0].models,
        Some(ModelAllowlist::new(["gpt-*", "o3"]).unwrap())
    );
    relay.state.set_account_models(&id, None).unwrap();
    relay.state.stop_background().await;
    upstream.verify().await;
}

#[tokio::test]
async fn config_default_models_apply_until_the_account_sets_its_own() {
    let upstream = MockServer::start().await;
    mock_usage(&upstream).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/ok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&upstream)
        .await;
    let base = format!("{}/backend-api", upstream.uri());
    let server = TestServer::start().await;
    let state = daemon_state(
        "relay-models-default",
        &server.url,
        "",
        &format!(
            r#"
            [models]
            codex = ["gpt-*"]
            [endpoints.codex]
            upstream_base = "{base}"
            token_url = "{0}/tokens"
            usage_url = "{0}/usage"
            "#,
            upstream.uri()
        ),
        true,
    );
    let (tunnel, _store) = Tunnel::with_codex_account(&state, server).await;
    let send = |body: &'static str| {
        tunnel
            .request(Method::POST, "/relay/codex-1/ok")
            .body(body)
            .send()
    };

    assert_eq!(send(r#"{"model": "gpt-6"}"#).await.status(), StatusCode::OK);
    let response = send(r#"{"model": "o3"}"#).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[DENIED_HEADER], DENIED_MODEL);

    let id = "codex-1".parse().unwrap();
    state
        .set_account_models(&id, Some(ModelAllowlist::new(["o3"]).unwrap()))
        .unwrap();
    assert_eq!(
        send(r#"{"model": "gpt-6"}"#).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    state.stop_background().await;
    upstream.verify().await;
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn upstream_401_refreshes_once_and_retries() {
    let upstream = MockServer::start().await;
    mock_usage(&upstream).await;
    for (token, status) in [("Bearer access", 401), ("Bearer fresh-access", 200)] {
        Mock::given(method("POST"))
            .and(path("/backend-api/request"))
            .and(header("authorization", token))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&upstream)
            .await;
    }
    mock_refresh(&upstream, "refresh", fresh_tokens()).await;
    let relay = Relay::start("relay-refresh", &upstream).await;
    assert_eq!(relay.request("request").await.status(), StatusCode::OK);
    let saved = relay.store.load_all().unwrap().remove(0);
    assert_eq!(
        saved.credentials.refresh_token.as_deref(),
        Some("fresh-refresh")
    );
    relay.state.stop_background().await;
    upstream.verify().await;
}

#[tokio::test]
async fn auth_failure_retries_with_rotated_disk_token_once_for_concurrent_callers() {
    let upstream = MockServer::start().await;
    mock_usage(&upstream).await;
    mock_refresh(
        &upstream,
        "refresh",
        ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_grant"})),
    )
    .await;
    mock_refresh(&upstream, "probe-refresh", fresh_tokens()).await;

    let relay = Relay::start("relay-rotation", &upstream).await;
    let mut disk = relay.store.load_all().unwrap().remove(0);
    let account = relay.state.account(&disk.id).unwrap();
    disk.credentials.access_token = "probe-access".to_owned();
    disk.credentials.refresh_token = Some("probe-refresh".to_owned());
    relay.store.save(&disk).unwrap();

    let (first, second) = tokio::join!(
        relay.state.fresh_credentials(&account, Some("access")),
        relay.state.fresh_credentials(&account, Some("access")),
    );
    assert_eq!(first.unwrap().access_token, "fresh-access");
    assert_eq!(second.unwrap().access_token, "fresh-access");
    assert!(account.inner().auth_error.is_none());
    let saved = relay.store.load_all().unwrap().remove(0);
    assert_eq!(
        saved.credentials.refresh_token.as_deref(),
        Some("fresh-refresh")
    );
    relay.state.stop_background().await;
    upstream.verify().await;
}

async fn mock_refresh(upstream: &MockServer, token: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/tokens"))
        .and(body_partial_json(json!({"refresh_token": token})))
        .respond_with(response)
        .expect(1)
        .mount(upstream)
        .await;
}

fn fresh_tokens() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": "fresh-access",
        "refresh_token": "fresh-refresh",
        "expires_in": 3600,
    }))
}

#[tokio::test]
async fn canceled_refresh_caller_does_not_lose_tokens_or_recreate_deleted_account() {
    let upstream = MockServer::start().await;
    mock_usage(&upstream).await;
    let refresh = Mock::given(method("POST"))
        .and(path("/tokens"))
        .respond_with(fresh_tokens().set_delay(Duration::from_millis(100)))
        .expect(1)
        .mount_as_scoped(&upstream)
        .await;

    let relay = Relay::start("relay-refresh-delete", &upstream).await;
    let id = "codex-1".parse().unwrap();
    let account = relay.state.account(&id).unwrap();
    let caller = {
        let state = relay.state.clone();
        let account = account.clone();
        tokio::spawn(async move { state.fresh_credentials(&account, Some("access")).await })
    };
    tokio::time::timeout(Duration::from_secs(5), refresh.wait_until_satisfied())
        .await
        .expect("refresh request reached the upstream");
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    relay.state.reload().await.unwrap();
    let saved = relay.store.load_all().unwrap().remove(0);
    assert_eq!(saved.credentials.access_token, "fresh-access");

    relay.state.delete_account(&id).await.unwrap();
    assert!(relay.state.account(&id).is_none());
    assert!(
        relay
            .state
            .fresh_credentials(&account, Some("fresh-access"))
            .await
            .is_err()
    );
    assert!(relay.store.load_all().unwrap().is_empty());
    relay.state.stop_background().await;
}

#[tokio::test]
async fn shutdown_cancels_an_in_flight_usage_poll() {
    let upstream = MockServer::start().await;
    let poll = Mock::given(method("GET"))
        .and(path("/usage"))
        .and(header("authorization", "Bearer access"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .expect(1)
        .mount_as_scoped(&upstream)
        .await;
    let relay = Relay::start("poller-shutdown", &upstream).await;
    tokio::time::timeout(Duration::from_secs(5), poll.wait_until_satisfied())
        .await
        .expect("usage request reached the configured upstream");
    tokio::time::timeout(Duration::from_secs(1), relay.state.stop_background())
        .await
        .expect("shutdown cancels the outstanding poll");
}

/// An upstream that echoes WebSocket messages, and refuses callers that do
/// not carry the account's own access token. Returns the base URL for
/// `upstream_base`.
async fn spawn_echo_upstream() -> String {
    async fn echo(upgrade: WebSocketUpgrade, headers: HeaderMap) -> Response {
        let authorized = headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer access");
        if !authorized {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        upgrade.on_upgrade(|mut socket| async move {
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

    let app = Router::new().route("/backend-api/socket", any(echo));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/backend-api", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Serve a consumer endpoint that opens `/relay/codex-1/socket` over the
/// relay's tunnel with the consumer's headers, and bridges to it.
async fn bridge_front(relay: &Relay) -> std::net::SocketAddr {
    let client = relay.tunnel.client.clone();
    let front = Router::new().route(
        "/ws",
        get(move |upgrade: WebSocketUpgrade, request: Request| {
            let client = client.clone();
            async move {
                let socket = match client
                    .connect_websocket("/relay/codex-1/socket", request.headers().clone())
                    .await
                {
                    Ok(socket) => socket,
                    Err(err) => return err.into_response(),
                };
                let mut extra = HeaderMap::new();
                if let Some(value) = socket.response_headers().get(HEADROOM_HEADER) {
                    extra.insert(HeaderName::from_static(HEADROOM_HEADER), value.clone());
                }
                socket.bridge(upgrade, extra, FrameHooks::default())
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, front).await.unwrap() });
    address
}

type Consumer =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn consumer(
    address: std::net::SocketAddr,
    model: Option<&'static str>,
) -> (Consumer, HeaderMap) {
    use tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", HeaderValue::from_static("Bearer consumer"));
    if let Some(model) = model {
        request
            .headers_mut()
            .insert(MODEL_HEADER, HeaderValue::from_static(model));
    }
    let (consumer, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    (consumer, response.headers().clone())
}

async fn next_message(consumer: &mut Consumer) -> tungstenite::Message {
    tokio::time::timeout(WAIT, consumer.next())
        .await
        .expect("a message arrives")
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn websocket_echoes_through_the_tunnel_to_the_upstream() {
    let mock = MockServer::start().await;
    mock_usage(&mock).await;
    let relay = Relay::start_at("relay-websocket", &mock, &spawn_echo_upstream().await).await;
    let (mut consumer, headers) = consumer(bridge_front(&relay).await, None).await;
    let headroom = headers[HEADROOM_HEADER].to_str().unwrap();
    assert_ne!(headroom, "deny");

    let hello = tungstenite::Message::Text("hello through the tunnel".into());
    consumer.send(hello.clone()).await.unwrap();
    assert_eq!(next_message(&mut consumer).await, hello);

    consumer
        .send(tungstenite::Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    let after = tungstenite::Message::Text("after a binary message".into());
    consumer.send(after.clone()).await.unwrap();
    assert_eq!(
        next_message(&mut consumer).await,
        after,
        "binary messages on a Codex socket never reach upstream"
    );
    consumer.close(None).await.unwrap();
    relay.state.stop_background().await;
}

#[tokio::test]
async fn websocket_models_the_account_does_not_allow_never_reach_upstream() {
    let mock = MockServer::start().await;
    mock_usage(&mock).await;
    let relay = Relay::start_at(
        "relay-websocket-models",
        &mock,
        &spawn_echo_upstream().await,
    )
    .await;
    let id = "codex-1".parse().unwrap();
    relay
        .state
        .set_account_models(&id, Some(ModelAllowlist::new(["gpt-*"]).unwrap()))
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(MODEL_HEADER, HeaderValue::from_static("o3"));
    let err = relay
        .tunnel
        .client
        .connect_websocket("/relay/codex-1/socket", headers)
        .await
        .err()
        .expect("a handshake for a disallowed model is refused");
    let mesh_relay::Error::Rejected {
        status, headers, ..
    } = err
    else {
        panic!("expected Rejected, got {err}");
    };
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers[DENIED_HEADER], DENIED_MODEL);

    let (mut consumer, _) = consumer(bridge_front(&relay).await, Some("gpt-6")).await;
    let create = |model: &str| {
        tungstenite::Message::Text(
            json!({"type": "response.create", "model": model, "input": []})
                .to_string()
                .into(),
        )
    };
    let interrupt = tungstenite::Message::Text(
        r#"{"type":"response.interrupt","response_id":"resp_1"}"#.into(),
    );
    for allowed in [create("gpt-6"), interrupt] {
        consumer.send(allowed.clone()).await.unwrap();
        assert_eq!(
            next_message(&mut consumer).await,
            allowed,
            "allowed messages pass unchanged"
        );
    }

    consumer.send(create("o3")).await.unwrap();
    let event = next_message(&mut consumer).await;
    let event: serde_json::Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["status"], 404);
    assert_eq!(event["error"]["code"], "model_not_found");

    consumer.send(create("gpt-7")).await.unwrap();
    assert_eq!(
        next_message(&mut consumer).await,
        create("gpt-7"),
        "the socket stays open after a refusal, and upstream never saw it"
    );
    relay.state.set_paused(&id, true).unwrap();
    consumer.send(create("gpt-7")).await.unwrap();
    let event = next_message(&mut consumer).await;
    let event: serde_json::Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
    assert_eq!(event["status"], 429);
    assert_eq!(event["error"]["message"], "account refused: paused");
    consumer.close(None).await.unwrap();
    relay.state.set_account_models(&id, None).unwrap();
    relay.state.stop_background().await;
}

#[tokio::test]
async fn websocket_to_an_unknown_account_is_refused_over_the_tunnel() {
    let mock = MockServer::start().await;
    mock_usage(&mock).await;
    let relay = Relay::start("relay-websocket-unknown", &mock).await;
    let err = relay
        .tunnel
        .client
        .connect_websocket("/relay/nope/socket", HeaderMap::new())
        .await
        .err()
        .expect("refusal");
    let mesh_relay::Error::Rejected { status, .. } = err else {
        panic!("expected Rejected, got {err}");
    };
    assert_eq!(status, StatusCode::NOT_FOUND);
    relay.state.stop_background().await;
}
