use chrono::{Duration, TimeZone, Utc};
use http::{HeaderMap, HeaderValue};
use mesh_core::Provider;
use mesh_providers::{Credentials, apply_auth};

fn credentials() -> Credentials {
    Credentials {
        access_token: "oauth-access".to_owned(),
        refresh_token: Some("oauth-refresh".to_owned()),
        expires_at: None,
        chatgpt_account_id: Some("acct-42".to_owned()),
        email: None,
    }
}

fn consumer_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer consumer-token"),
        ("x-api-key", "sk-consumer"),
        ("chatgpt-account-id", "consumer-account"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", "claude-code-20250219"),
        ("user-agent", "claude-cli/2.0.0"),
        ("x-custom", "kept"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers
}

#[test]
fn claude_bearer_replaces_consumer_credentials() {
    let mut headers = consumer_headers();
    apply_auth(Provider::Claude, &credentials(), &mut headers);

    assert_eq!(headers["authorization"], "Bearer oauth-access");
    assert!(headers["authorization"].is_sensitive());
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(headers.get_all("authorization").iter().count(), 1);
    assert_eq!(
        headers["anthropic-beta"],
        "claude-code-20250219,oauth-2025-04-20"
    );
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["user-agent"], "claude-cli/2.0.0");
    assert_eq!(headers["x-custom"], "kept");
}

#[test]
fn claude_adds_the_oauth_beta_when_the_consumer_sent_none() {
    let mut headers = HeaderMap::new();
    apply_auth(Provider::Claude, &credentials(), &mut headers);
    assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
    assert_eq!(headers["authorization"], "Bearer oauth-access");
}

#[test]
fn codex_sets_bearer_and_account_id() {
    let mut headers = consumer_headers();
    apply_auth(Provider::Codex, &credentials(), &mut headers);

    assert_eq!(headers["authorization"], "Bearer oauth-access");
    assert_eq!(headers["chatgpt-account-id"], "acct-42");
    assert!(!headers.contains_key("x-api-key"));
    assert!(
        !headers.contains_key("anthropic-beta")
            || headers["anthropic-beta"] == "claude-code-20250219"
    );
    assert_eq!(headers["x-custom"], "kept");
}

#[test]
fn codex_without_an_account_id_drops_the_consumers() {
    let mut credentials = credentials();
    credentials.chatgpt_account_id = None;
    let mut headers = consumer_headers();
    apply_auth(Provider::Codex, &credentials, &mut headers);
    assert!(!headers.contains_key("chatgpt-account-id"));
}

#[test]
fn needs_refresh_uses_a_five_minute_margin() {
    let now = Utc.with_ymd_and_hms(2030, 1, 1, 12, 0, 0).unwrap();
    let with_expiry = |expires_at| Credentials {
        expires_at,
        ..credentials()
    };

    assert!(!with_expiry(None).needs_refresh(now));
    assert!(!with_expiry(Some(now + Duration::minutes(6))).needs_refresh(now));
    assert!(with_expiry(Some(now + Duration::minutes(5))).needs_refresh(now));
    assert!(with_expiry(Some(now + Duration::minutes(4))).needs_refresh(now));
    assert!(with_expiry(Some(now)).needs_refresh(now));
    assert!(with_expiry(Some(now - Duration::hours(1))).needs_refresh(now));
}

#[test]
fn debug_output_hides_tokens() {
    let text = format!("{:?}", credentials());
    assert!(!text.contains("oauth-access"));
    assert!(!text.contains("oauth-refresh"));
    assert!(text.contains("acct-42"));
}
