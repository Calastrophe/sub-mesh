use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use chrono::TimeDelta;
use mesh_core::{AccountId, Provider, Score};
use serde_json::{Value, json};

use super::{
    MAX_LIMITED_COOLDOWN, limited_cooldown, model_not_found, model_not_found_body, refusal_body,
    relay_target, unavailable, upstream_path, valid_endpoint, valid_relay_path,
};
use crate::registry::{Candidate, DEFAULT_LIMITED_COOLDOWN};
use axum::http::Method;

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(*name, HeaderValue::from_static(value));
    }
    map
}

fn candidate() -> Candidate {
    Candidate {
        id: "claude-ab12".parse::<AccountId>().unwrap(),
        node: "n1".to_owned(),
        owner: None,
        generation: 1,
        headroom: 0.5,
        score: Score {
            value: 1.0,
            min_room: 0.5,
        },
    }
}

#[test]
fn claude_503_body_shape() {
    let body = refusal_body(Provider::Claude, "mesh: no account has headroom");
    assert_eq!(
        body,
        json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": "mesh: no account has headroom"
            }
        })
    );
}

#[test]
fn codex_503_body_shape() {
    let body = refusal_body(Provider::Codex, "mesh: no account has headroom");
    assert_eq!(
        body,
        json!({
            "error": {
                "type": "mesh_unavailable",
                "message": "mesh: no account has headroom"
            }
        })
    );
}

#[tokio::test]
async fn unavailable_is_a_json_503() {
    let response = unavailable(Provider::Claude, "nope");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn an_incomplete_upload_times_out() {
    let body = axum::body::Body::from_stream(futures_util::stream::pending::<
        Result<bytes::Bytes, std::io::Error>,
    >());
    let error = super::http::read_body(
        &HeaderMap::new(),
        body,
        1024,
        std::time::Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert_eq!(error.0, StatusCode::REQUEST_TIMEOUT);
}

#[test]
fn builds_relay_targets() {
    assert_eq!(
        relay_target(&candidate(), "/v1/messages", Some("beta=true")),
        "/relay/claude-ab12/v1/messages?beta=true"
    );
    assert_eq!(
        relay_target(&candidate(), "/v1/responses", None),
        "/relay/claude-ab12/v1/responses"
    );
    assert_eq!(relay_target(&candidate(), "", None), "/relay/claude-ab12");
}

#[test]
fn relay_paths_must_not_escape_the_relay_route() {
    for path in [
        "/../responses",
        "/%2E%2e/responses",
        "/v1/./responses",
        r"/a\b",
        "/v1/data%2Fitem",
        "/v1/data%5citem",
        "//other-host/v1",
        "/scheme:relative",
    ] {
        assert!(!valid_relay_path(path), "{path:?}");
    }
    assert!(valid_relay_path("/v1/data%20item"));
    assert_eq!(
        relay_target(&candidate(), "/v1/data%20item", Some("q=a%2Bb")),
        "/relay/claude-ab12/v1/data%20item?q=a%2Bb"
    );
}

#[test]
fn only_supported_provider_endpoints_and_methods_are_relayed() {
    for (provider, method, path, websocket, expected) in [
        (Provider::Claude, Method::POST, "/v1/messages", false, true),
        (Provider::Claude, Method::GET, "/v1/models", false, true),
        (
            Provider::Claude,
            Method::POST,
            "/v1/messages/count_tokens",
            false,
            false,
        ),
        (Provider::Claude, Method::GET, "/v1/messages", false, false),
        (Provider::Codex, Method::POST, "/responses", false, true),
        (Provider::Codex, Method::GET, "/responses", true, true),
        (
            Provider::Codex,
            Method::POST,
            "/responses/compact",
            false,
            true,
        ),
        (
            Provider::Codex,
            Method::GET,
            "/responses/compact",
            true,
            false,
        ),
        (Provider::Codex, Method::GET, "/models", false, true),
        (Provider::Codex, Method::POST, "/v1/realtime", false, false),
        (Provider::Codex, Method::POST, "/videos", false, false),
        (Provider::Codex, Method::POST, "/v1/tunnel", false, false),
    ] {
        assert_eq!(
            valid_endpoint(provider, &method, path, websocket),
            expected,
            "{provider} {method} {path}"
        );
    }
}

#[test]
fn strips_the_provider_prefix() {
    assert_eq!(
        upstream_path("/claude/v1/messages", Provider::Claude),
        "/v1/messages"
    );
    assert_eq!(
        upstream_path("/codex/responses", Provider::Codex),
        "/responses"
    );
    assert_eq!(upstream_path("/claude", Provider::Claude), "");
}

#[test]
fn retry_after_is_capped_with_a_default() {
    let codex = |pairs| limited_cooldown(Provider::Codex, &headers(pairs));
    assert_eq!(codex(&[]), Some(DEFAULT_LIMITED_COOLDOWN));
    assert_eq!(
        codex(&[("retry-after", "120")]),
        Some(TimeDelta::seconds(120))
    );
    assert_eq!(
        codex(&[("retry-after", "999999")]),
        Some(MAX_LIMITED_COOLDOWN)
    );
    assert_eq!(
        codex(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]),
        Some(DEFAULT_LIMITED_COOLDOWN)
    );
}

#[test]
fn only_a_rejected_claude_subscription_rests() {
    let claude = |pairs| limited_cooldown(Provider::Claude, &headers(pairs));
    assert_eq!(claude(&[]), None);
    assert_eq!(
        claude(&[("retry-after", "30"), ("x-should-retry", "true")]),
        None
    );
    assert_eq!(
        claude(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ]),
        None
    );
    for name in [
        "anthropic-ratelimit-unified-status",
        "anthropic-ratelimit-unified-5h-status",
        "anthropic-ratelimit-unified-7d-status",
    ] {
        assert_eq!(
            limited_cooldown(Provider::Claude, &headers(&[(name, "rejected")])),
            Some(DEFAULT_LIMITED_COOLDOWN),
            "{name}"
        );
    }
    assert_eq!(
        claude(&[
            ("anthropic-ratelimit-unified-status", "Rejected"),
            ("retry-after", "120"),
        ]),
        Some(TimeDelta::seconds(120))
    );
}

#[test]
fn claude_model_not_found_body_shape() {
    assert_eq!(
        model_not_found_body(Provider::Claude, Some("claude-x")),
        json!({
            "type": "error",
            "error": {
                "type": "not_found_error",
                "message": "mesh: no account may serve the model claude-x"
            }
        })
    );
}

#[test]
fn codex_model_not_found_body_shape() {
    assert_eq!(
        model_not_found_body(Provider::Codex, Some("gpt-x")),
        json!({
            "error": {
                "message": "mesh: no account may serve the model gpt-x",
                "type": "invalid_request_error",
                "code": "model_not_found"
            }
        })
    );
}

#[test]
fn an_unknown_model_gets_a_message_without_it() {
    for provider in Provider::ALL {
        let body = model_not_found_body(provider, None).to_string();
        assert!(body.contains("mesh: no account may serve the requested model"));
    }
}

#[tokio::test]
async fn model_not_found_is_a_json_404() {
    let response = model_not_found(Provider::Codex, Some("a\"b"));
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], "model_not_found");
    assert!(value["error"]["message"].as_str().unwrap().contains("a\"b"));
}
