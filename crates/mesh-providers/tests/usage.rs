use chrono::{DateTime, Utc};
use mesh_core::Provider;
use mesh_providers::{Credentials, Endpoints, ProviderError, poll_usage_with};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn credentials() -> Credentials {
    Credentials {
        access_token: "access-token".to_owned(),
        refresh_token: None,
        expires_at: None,
        chatgpt_account_id: Some("acct-9".to_owned()),
        email: None,
    }
}

fn endpoints(provider: Provider, server: &MockServer, usage_path: &str) -> Endpoints {
    let mut endpoints = Endpoints::new(provider);
    endpoints.usage_url = format!("{}{usage_path}", server.uri()).parse().unwrap();
    endpoints
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

#[tokio::test]
async fn codex_usage_uses_the_account_headers_and_maps_windows() {
    let server = MockServer::start().await;
    let body = json!({
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 40, "limit_window_seconds": 18000,
                               "reset_after_seconds": 100, "reset_at": 1_700_000_100},
            "secondary_window": {"used_percent": 8, "limit_window_seconds": 604_800,
                                 "reset_after_seconds": 5000, "reset_at": 1_700_005_000}
        },
        "additional_rate_limits": [{
            "limit_name": "Spark", "metered_feature": "codex_spark",
            "rate_limit": {"primary_window": {"used_percent": 1, "limit_window_seconds": 3600,
                                              "reset_after_seconds": 1, "reset_at": 1_700_000_001}}
        }]
    });
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("authorization", "Bearer access-token"))
        .and(header("chatgpt-account-id", "acct-9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;
    let endpoints = endpoints(Provider::Codex, &server, "/backend-api/wham/usage");
    let client = reqwest::Client::new();

    let usage = poll_usage_with(&client, &credentials(), &endpoints, now())
        .await
        .unwrap();
    assert_eq!(usage.raw, body);
    let windows = &usage.windows;
    let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["primary", "secondary"]);
    assert_eq!(windows[0].duration_minutes, Some(300));
    assert!((windows[0].used - 0.4).abs() < 1e-9);
    assert_eq!(
        windows[0].resets_at,
        DateTime::from_timestamp(1_700_000_100, 0)
    );
}

#[tokio::test]
async fn claude_usage_sends_the_oauth_beta_and_no_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .and(header("authorization", "Bearer access-token"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour": {"utilization": 22.0, "resets_at": "2023-11-14T23:00:00+00:00"},
            "seven_day": {"utilization": 61.5, "resets_at": "2023-11-20T00:00:00Z"},
            "seven_day_opus": null,
            "seven_day_sonnet": {"utilization": 3, "resets_at": null}
        })))
        .mount(&server)
        .await;
    let endpoints = endpoints(Provider::Claude, &server, "/api/oauth/usage");

    let windows = poll_usage_with(&reqwest::Client::new(), &credentials(), &endpoints, now())
        .await
        .unwrap()
        .windows;
    let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["five_hour", "seven_day"]);
    assert!((windows[1].used - 0.615).abs() < 1e-9);
    assert_eq!(
        windows[0].resets_at,
        DateTime::from_timestamp(1_700_002_800, 0)
    );

    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("x-api-key"));
    assert!(!requests[0].headers.contains_key("chatgpt-account-id"));
}

#[tokio::test]
async fn http_errors_carry_the_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired: secret-access-token"))
        .mount(&server)
        .await;
    let endpoints = endpoints(Provider::Claude, &server, "/api/oauth/usage");
    let error = poll_usage_with(&reqwest::Client::new(), &credentials(), &endpoints, now())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ProviderError::Status { status: 401, body } if !body.contains("secret-access-token")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_non_json_body_is_a_json_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let endpoints = endpoints(Provider::Codex, &server, "/usage");
    let error = poll_usage_with(&reqwest::Client::new(), &credentials(), &endpoints, now())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Json(_)), "{error:?}");
}
