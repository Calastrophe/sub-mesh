use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn parses_every_pasted_form() {
    for (input, expected) in [
        (
            "http://localhost:1455/auth/callback?code=abc&state=xyz\n",
            "abc",
        ),
        ("?code=abc&state=xyz", "abc"),
        ("abc#xyz", "abc"),
        ("  abc  ", "abc"),
        ("http://localhost/cb?code=a%2Bb&state=xyz", "a+b"),
    ] {
        assert_eq!(parse_pasted(input, "xyz").unwrap(), expected);
    }

    for invalid in [
        "   ",
        "#xyz",
        "abc#",
        "http://localhost/cb?code=abc",
        "http://localhost/cb?state=xyz",
        "?code=abc&state=xyz&state=xyz",
    ] {
        assert!(parse_pasted(invalid, "xyz").is_err(), "{invalid}");
    }
    let denied = parse_pasted("http://localhost/cb?error=access_denied&state=xyz", "xyz")
        .unwrap_err()
        .to_string();
    assert!(denied.contains("access_denied"), "{denied}");
}

#[test]
fn account_ids_are_valid() {
    for provider in Provider::ALL {
        let id = new_account_id(provider);
        let (prefix, hex) = id.as_str().split_once('-').unwrap();
        assert_eq!(prefix, provider.as_str());
        assert_eq!(hex.len(), 8);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}

/// What a browser does when the provider redirects it to the local callback.
async fn browser_visit(redirect_uri: &str, query: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let port = Url::parse(redirect_uri).unwrap().port().unwrap();
    let path = Url::parse(redirect_uri).unwrap().path().to_owned();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request = format!("GET {path}?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

fn mock_endpoints(provider: Provider, server: &MockServer) -> Endpoints {
    let mut endpoints = Endpoints::new(provider);
    endpoints.token_url = format!("{}/token", server.uri()).parse().unwrap();
    endpoints.callback_port = 0;
    endpoints
}

fn query(url: &Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .unwrap_or_else(|| panic!("{key} in {url}"))
        .1
        .into_owned()
}

#[tokio::test]
async fn codex_login_through_the_callback() {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use sha2::{Digest, Sha256};

    let server = MockServer::start().await;
    let id_token = crate::jwt::tests::make_jwt(&serde_json::json!({
        "email": "me@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-77"},
    }));
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-1", "refresh_token": "refresh-1",
            "id_token": id_token, "expires_in": 3600,
        })))
        .mount(&server)
        .await;

    let endpoints = mock_endpoints(Provider::Codex, &server);
    let mut flow = LoginFlow::begin(&endpoints, false).await.unwrap();

    let url = flow.authorize_url().clone();
    assert_eq!(url.host_str(), Some("auth.openai.com"));
    assert_eq!(query(&url, "client_id"), "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(query(&url, "response_type"), "code");
    assert_eq!(query(&url, "code_challenge_method"), "S256");
    assert_eq!(query(&url, "redirect_uri"), flow.redirect_uri);
    assert!(flow.redirect_uri.starts_with("http://localhost:"));
    assert!(flow.redirect_uri.ends_with("/auth/callback"));
    let state = query(&url, "state");

    let redirect = flow.redirect_uri.clone();
    let browser = tokio::spawn(async move {
        browser_visit(&redirect, &format!("code=the-code&state={state}")).await;
    });
    let code = flow
        .wait_for_callback(Duration::from_secs(10))
        .await
        .unwrap();
    browser.await.unwrap();
    assert_eq!(code, "the-code");

    let account = flow.complete(&reqwest::Client::new(), &code).await.unwrap();
    assert_eq!(account.provider, Provider::Codex);
    assert!(account.id.as_str().starts_with("codex-"));
    assert_eq!(account.label.as_deref(), Some("me@example.com"));
    assert_eq!(account.credentials.access_token, "access-1");
    assert_eq!(
        account.credentials.refresh_token.as_deref(),
        Some("refresh-1")
    );
    assert_eq!(
        account.credentials.chatgpt_account_id.as_deref(),
        Some("acct-77")
    );
    assert!(account.credentials.expires_at.unwrap() > Utc::now());

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(&requests[0].body)
            .into_owned()
            .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(form["redirect_uri"], flow.redirect_uri);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes()));
    assert_eq!(challenge, query(&url, "code_challenge"));
}

#[tokio::test]
async fn claude_login_with_a_pasted_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "sk-ant-oat-1", "refresh_token": "sk-ant-ort-1", "expires_in": 28800,
            "account": {"uuid": "u-1", "email_address": "claude@example.com"},
            "organization": {"uuid": "o-1", "name": "Org"},
        })))
        .mount(&server)
        .await;

    let endpoints = mock_endpoints(Provider::Claude, &server);
    let flow = LoginFlow::begin(&endpoints, true).await.unwrap();
    let url = flow.authorize_url().clone();
    assert_eq!(url.host_str(), Some("claude.ai"));
    assert_eq!(
        query(&url, "client_id"),
        "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
    );
    assert_eq!(query(&url, "code"), "true");
    assert!(query(&url, "scope").contains("user:inference"));
    assert_eq!(flow.redirect_uri, "http://localhost:0/callback");

    let pasted = format!(
        "http://localhost:54545/callback?code=auth-code&state={}\n",
        query(&url, "state")
    );
    let code = flow
        .read_code(&mut pasted.as_bytes(), Duration::from_secs(5))
        .await
        .unwrap();
    let account = flow.complete(&reqwest::Client::new(), &code).await.unwrap();

    assert!(account.id.as_str().starts_with("claude-"));
    assert_eq!(account.label.as_deref(), Some("claude@example.com"));
    assert_eq!(account.credentials.access_token, "sk-ant-oat-1");
    assert_eq!(
        account.credentials.refresh_token.as_deref(),
        Some("sk-ant-ort-1")
    );
    assert_eq!(account.credentials.chatgpt_account_id, None);

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "auth-code");
    assert_eq!(body["state"], query(&url, "state"));
    assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(body["redirect_uri"], flow.redirect_uri);
    assert_eq!(body["code_verifier"].as_str().unwrap().len(), 43);
}

#[tokio::test]
async fn a_failed_exchange_is_a_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid_grant"}"#))
        .mount(&server)
        .await;
    let endpoints = mock_endpoints(Provider::Codex, &server);
    let flow = LoginFlow::begin(&endpoints, true).await.unwrap();
    let error = flow
        .complete(&reqwest::Client::new(), "bad")
        .await
        .unwrap_err();
    assert!(
        matches!(error, ProviderError::Status { status: 400, .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn manual_mode_has_no_callback_server() {
    let endpoints = Endpoints::new(Provider::Claude);
    let mut flow = LoginFlow::begin(&endpoints, true).await.unwrap();
    assert_eq!(flow.redirect_uri, "http://localhost:54545/callback");
    assert!(
        flow.wait_for_callback(Duration::from_millis(10))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn manual_input_checks_state() {
    let mut endpoints = Endpoints::new(Provider::Claude);
    endpoints.callback_port = 0;
    let flow = LoginFlow::begin(&endpoints, true).await.unwrap();
    let limit = Duration::from_secs(5);

    let good = format!(
        "http://localhost:54545/callback?code=c1&state={}\n",
        flow.state
    );
    assert_eq!(
        flow.read_code(&mut good.as_bytes(), limit).await.unwrap(),
        "c1"
    );
    assert_eq!(
        flow.read_code(&mut &b"bare-code\n"[..], limit)
            .await
            .unwrap(),
        "bare-code"
    );

    let wrong = "http://localhost:54545/callback?code=c1&state=nope\n";
    let error = flow
        .read_code(&mut wrong.as_bytes(), limit)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("state mismatch"), "{error}");

    let error = flow.read_code(&mut &b""[..], limit).await.unwrap_err();
    assert!(error.to_string().contains("no code"), "{error}");
}
