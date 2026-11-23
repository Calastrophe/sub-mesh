use mesh_core::Provider;
use mesh_providers::{upstream_base, upstream_url};

#[test]
fn bases() {
    assert_eq!(
        upstream_base(Provider::Claude).as_str(),
        "https://api.anthropic.com/"
    );
    assert_eq!(
        upstream_base(Provider::Codex).as_str(),
        "https://chatgpt.com/backend-api/codex/"
    );
}

#[test]
fn maps_consumer_paths_to_upstream_urls() {
    let url = |provider, path: &str| upstream_url(provider, path).unwrap().to_string();
    assert_eq!(
        url(Provider::Codex, ""),
        "https://chatgpt.com/backend-api/codex/"
    );
    assert_eq!(
        url(Provider::Claude, "v1/messages?beta=true"),
        "https://api.anthropic.com/v1/messages?beta=true"
    );
    assert_eq!(
        url(Provider::Claude, "/v1/messages/count_tokens"),
        "https://api.anthropic.com/v1/messages/count_tokens"
    );
    assert_eq!(
        url(Provider::Codex, "responses"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        url(Provider::Codex, "/models?client_version=1.2.3"),
        "https://chatgpt.com/backend-api/codex/models?client_version=1.2.3"
    );
}

#[test]
fn rejects_paths_that_escape_the_base() {
    for provider in Provider::ALL {
        for bad in [
            "https://evil.example/x",
            "http://evil.example",
            "//evil.example/x",
            "///evil.example",
            "http:evil.example",
            "/\\evil.example",
            "../secret",
            "./x",
            "v1/../../secret",
            "v1/%2e%2E/secret",
            "v1/%2E./x",
            "v1/a%5cb",
            "v1/..%2f..%2fsecret",
            "v1/messages#fragment",
            "v1/mess ages",
            "v1/messages\r\nHost: evil.example",
            "evil.example:443/x",
        ] {
            assert!(
                upstream_url(provider, bad).is_err(),
                "{provider}: {bad:?} should be rejected"
            );
        }
    }
}

#[test]
fn codex_cannot_reach_account_routes() {
    for bad in [
        "../wham/usage",
        "../../backend-api/ps/mcp",
        "%2e%2e/wham/settings/user",
    ] {
        assert!(
            upstream_url(Provider::Codex, bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn a_query_may_hold_urls_and_dots() {
    let url = upstream_url(Provider::Claude, "v1/messages?next=https://x.test/../y").unwrap();
    assert_eq!(url.host_str(), Some("api.anthropic.com"));
    assert_eq!(url.path(), "/v1/messages");
}
