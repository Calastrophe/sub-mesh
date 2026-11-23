use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use mesh_core::Provider;
use mesh_providers::{Credentials, Endpoints, ProviderError, refresh_with};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn credentials(refresh_token: Option<&str>) -> Credentials {
    Credentials {
        access_token: "old-access".to_owned(),
        refresh_token: refresh_token.map(str::to_owned),
        expires_at: Some(Utc::now() - Duration::minutes(1)),
        chatgpt_account_id: Some("old-account".to_owned()),
        email: Some("old@example.com".to_owned()),
    }
}

fn endpoints(provider: Provider, server: &MockServer) -> Endpoints {
    let mut endpoints = Endpoints::new(provider);
    endpoints.token_url = format!("{}/oauth/token", server.uri()).parse().unwrap();
    endpoints
}

fn jwt(claims: &Value) -> String {
    let encode = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!("{}.{}.sig", encode(&json!({"alg": "none"})), encode(claims))
}

async fn mock_token(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn only_request_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    serde_json::from_slice(&requests[0].body).unwrap()
}

#[tokio::test]
async fn codex_refresh_rotates_tokens_and_reads_the_id_token() {
    let server = MockServer::start().await;
    let id_token = jwt(&json!({
        "email": "new@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "new-account"},
    }));
    mock_token(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "id_token": id_token,
            "expires_in": 3600,
            "token_type": "Bearer",
        })),
    )
    .await;

    let mut credentials = credentials(Some("old-refresh"));
    let before = Utc::now();
    refresh_with(
        &reqwest::Client::new(),
        &mut credentials,
        &endpoints(Provider::Codex, &server),
    )
    .await
    .unwrap();

    assert_eq!(credentials.access_token, "new-access");
    assert_eq!(credentials.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(
        credentials.chatgpt_account_id.as_deref(),
        Some("new-account")
    );
    assert_eq!(credentials.email.as_deref(), Some("new@example.com"));
    let expires_at = credentials.expires_at.unwrap();
    assert!(expires_at >= before + Duration::seconds(3600));
    assert!(expires_at <= Utc::now() + Duration::seconds(3600));

    assert_eq!(
        only_request_body(&server).await,
        json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": "old-refresh",
        })
    );
}

#[tokio::test]
async fn claude_refresh_sends_scope_and_keeps_the_old_token_when_none_is_returned() {
    let server = MockServer::start().await;
    mock_token(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access",
            "expires_in": 28800,
            "account": {"uuid": "u", "email_address": "claude@example.com"},
        })),
    )
    .await;

    let mut credentials = credentials(Some("old-refresh"));
    refresh_with(
        &reqwest::Client::new(),
        &mut credentials,
        &endpoints(Provider::Claude, &server),
    )
    .await
    .unwrap();

    assert_eq!(credentials.access_token, "new-access");
    assert_eq!(credentials.refresh_token.as_deref(), Some("old-refresh"));
    assert_eq!(credentials.email.as_deref(), Some("claude@example.com"));
    assert_eq!(
        credentials.chatgpt_account_id.as_deref(),
        Some("old-account"),
        "untouched"
    );

    let body = only_request_body(&server).await;
    assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "old-refresh");
    assert!(body["scope"].as_str().unwrap().contains("user:inference"));
}

#[tokio::test]
async fn rejected_refresh_tokens_are_auth_errors() {
    let cases = [
        (
            400,
            json!({"error": "invalid_grant", "error_description": "Refresh token not found: old-refresh"}),
        ),
        (401, json!({"error": "unauthorized"})),
        (
            400,
            json!({"error": {"code": "refresh_token_reused", "message": "used"}}),
        ),
        (
            400,
            json!({"type": "error", "error": {"type": "invalid_grant", "message": "bad"}}),
        ),
    ];
    for provider in Provider::ALL {
        for (status, body) in &cases {
            let server = MockServer::start().await;
            mock_token(&server, ResponseTemplate::new(*status).set_body_json(body)).await;
            let mut credentials = credentials(Some("old-refresh"));
            let error = refresh_with(
                &reqwest::Client::new(),
                &mut credentials,
                &endpoints(provider, &server),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, ProviderError::Auth(_)),
                "{provider} {status} {body}: {error:?}"
            );
            assert!(!format!("{error:?} {error}").contains("old-refresh"));
            assert_eq!(
                credentials.access_token, "old-access",
                "failure leaves credentials alone"
            );
            assert_eq!(credentials.refresh_token.as_deref(), Some("old-refresh"));
        }
    }
}

#[tokio::test]
async fn other_failures_are_not_auth_errors() {
    for (status, body) in [
        (500, "boom"),
        (429, "slow down"),
        (400, r#"{"error":"invalid_request"}"#),
    ] {
        let server = MockServer::start().await;
        mock_token(&server, ResponseTemplate::new(status).set_body_string(body)).await;
        let mut credentials = credentials(Some("old-refresh"));
        let error = refresh_with(
            &reqwest::Client::new(),
            &mut credentials,
            &endpoints(Provider::Claude, &server),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, ProviderError::Status { status: s, .. } if *s == status),
            "{status}: {error:?}"
        );
    }
}

#[tokio::test]
async fn missing_refresh_token_is_an_auth_error_without_a_request() {
    let server = MockServer::start().await;
    let mut credentials = credentials(None);
    let error = refresh_with(
        &reqwest::Client::new(),
        &mut credentials,
        &endpoints(Provider::Codex, &server),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProviderError::Auth(_)), "{error:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bad_success_bodies_are_errors() {
    for body in [json!({"refresh_token": "r"}), json!({"access_token": ""})] {
        let server = MockServer::start().await;
        mock_token(&server, ResponseTemplate::new(200).set_body_json(&body)).await;
        let mut credentials = credentials(Some("old-refresh"));
        let result = refresh_with(
            &reqwest::Client::new(),
            &mut credentials,
            &endpoints(Provider::Codex, &server),
        )
        .await;
        assert!(result.is_err(), "{body}");
        assert_eq!(credentials.access_token, "old-access");
    }
}

#[tokio::test]
async fn token_requests_identify_mesh_not_a_consumer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(header("accept", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "a", "expires_in": 60})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut credentials = credentials(Some("old-refresh"));
    refresh_with(
        &reqwest::Client::new(),
        &mut credentials,
        &endpoints(Provider::Claude, &server),
    )
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    let user_agent = requests[0]
        .headers
        .get("user-agent")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(user_agent.starts_with("mesh/"), "{user_agent}");
}

#[tokio::test]
async fn oversized_token_response_keeps_existing_credentials() {
    let server = MockServer::start().await;
    mock_token(
        &server,
        ResponseTemplate::new(200).set_body_raw(vec![b' '; 1024 * 1024 + 1], "application/json"),
    )
    .await;
    let mut credentials = credentials(Some("old-refresh"));
    let result = refresh_with(
        &reqwest::Client::new(),
        &mut credentials,
        &endpoints(Provider::Codex, &server),
    )
    .await;
    assert!(matches!(result, Err(ProviderError::Other(message)) if message.contains("too large")));
    assert_eq!(credentials.access_token, "old-access");
    assert_eq!(credentials.refresh_token.as_deref(), Some("old-refresh"));
}
