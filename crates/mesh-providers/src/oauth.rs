//! The OAuth plumbing shared by both providers: authorize URLs, the token
//! endpoint (code exchange and refresh), and error classification.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use http::header::ACCEPT;
use mesh_core::Provider;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::{Credentials, Endpoints, ProviderError, claude, codex, jwt};

/// Timeout for one request to a provider endpoint.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Sent on requests mesh makes for itself (token, usage). It is not sent on
/// relayed consumer requests.
pub(crate) const USER_AGENT: &str = concat!("mesh/", env!("CARGO_PKG_VERSION"));

/// How a token endpoint expects its request body.
#[derive(Clone, Copy)]
pub(crate) enum Encoding {
    Form,
    Json,
}

/// The fixed OAuth client settings of one provider.
pub(crate) struct OAuthApp {
    pub(crate) client_id: &'static str,
    pub(crate) scope: &'static str,
    /// Scope sent on refresh, when the provider wants one.
    pub(crate) refresh_scope: Option<&'static str>,
    /// Path of the local callback, which is part of the registered redirect URI.
    pub(crate) callback_path: &'static str,
    pub(crate) extra_authorize_params: &'static [(&'static str, &'static str)],
    pub(crate) exchange_encoding: Encoding,
    /// Whether the code exchange body repeats the `state`.
    pub(crate) exchange_sends_state: bool,
}

pub(crate) fn app(provider: Provider) -> &'static OAuthApp {
    match provider {
        Provider::Codex => &codex::APP,
        Provider::Claude => &claude::APP,
    }
}

/// What a token response says about who logged in.
#[derive(Default)]
pub(crate) struct Identity {
    pub(crate) chatgpt_account_id: Option<String>,
    pub(crate) email: Option<String>,
}

/// A successful token endpoint response. Has no `Debug` on purpose: it
/// holds secrets.
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    pub(crate) id_token: Option<String>,
    expires_in: Option<i64>,
    /// Claude returns the account here.
    pub(crate) account: Option<TokenAccount>,
}

#[derive(Deserialize)]
pub(crate) struct TokenAccount {
    pub(crate) email_address: Option<String>,
}

/// Used when the provider names no lifetime at all; refreshing early is cheap.
const FALLBACK_LIFETIME: TimeDelta = TimeDelta::hours(1);

impl TokenResponse {
    fn identity(&self, provider: Provider) -> Identity {
        match provider {
            Provider::Codex => codex::identity(self),
            Provider::Claude => claude::identity(self),
        }
    }

    /// `expires_in` when present, else the access token's own `exp` claim.
    fn expires_at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        if let Some(expires_at) = self
            .expires_in
            .filter(|seconds| *seconds > 0)
            .and_then(TimeDelta::try_seconds)
            .and_then(|lifetime| now.checked_add_signed(lifetime))
        {
            return expires_at;
        }
        jwt::expiry(&self.access_token).unwrap_or_else(|| {
            tracing::debug!("token response named no lifetime; assuming one hour");
            now.checked_add_signed(FALLBACK_LIFETIME)
                .unwrap_or(DateTime::<Utc>::MAX_UTC)
        })
    }

    /// Build credentials for a new login.
    pub(crate) fn into_credentials(
        self,
        provider: Provider,
        now: DateTime<Utc>,
    ) -> Result<Credentials, ProviderError> {
        if self.access_token.is_empty() {
            return Err(ProviderError::Other(
                "the token endpoint returned an empty access token".to_owned(),
            ));
        }
        let identity = self.identity(provider);
        Ok(Credentials {
            expires_at: Some(self.expires_at(now)),
            access_token: self.access_token,
            refresh_token: self.refresh_token.filter(|token| !token.is_empty()),
            chatgpt_account_id: identity.chatgpt_account_id,
            email: identity.email,
        })
    }
}

/// The browser URL that starts a login.
pub(crate) fn authorize_url(
    endpoints: &Endpoints,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Url {
    let app = app(endpoints.provider);
    let mut url = endpoints.authorize_url.clone();
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", app.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", app.scope)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        for (key, value) in app.extra_authorize_params {
            query.append_pair(key, value);
        }
    }
    url
}

/// The values of one authorization-code exchange.
pub(crate) struct CodeExchange<'a> {
    pub(crate) redirect_uri: &'a str,
    pub(crate) verifier: &'a str,
    pub(crate) state: &'a str,
    pub(crate) code: &'a str,
}

/// Trade an authorization code for tokens.
pub(crate) async fn exchange_code(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    exchange: &CodeExchange<'_>,
) -> Result<TokenResponse, ProviderError> {
    let app = app(endpoints.provider);
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", exchange.code),
        ("redirect_uri", exchange.redirect_uri),
        ("client_id", app.client_id),
        ("code_verifier", exchange.verifier),
    ];
    if app.exchange_sends_state {
        params.push(("state", exchange.state));
    }
    post_token(client, &endpoints.token_url, app.exchange_encoding, &params).await
}

/// Refresh `credentials` in place. A rejected refresh token becomes
/// [`ProviderError::Auth`].
pub(crate) async fn refresh(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    credentials: &mut Credentials,
) -> Result<(), ProviderError> {
    let app = app(endpoints.provider);
    let Some(refresh_token) = credentials.refresh_token.as_deref() else {
        return Err(ProviderError::Auth(
            "no refresh token is stored for this account".to_owned(),
        ));
    };
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("client_id", app.client_id),
        ("refresh_token", refresh_token),
    ];
    if let Some(scope) = app.refresh_scope {
        params.push(("scope", scope));
    }

    let now = Utc::now();
    let tokens = match post_token(client, &endpoints.token_url, Encoding::Json, &params).await {
        Ok(tokens) => tokens,
        Err(ProviderError::Status { status, body })
            if status == 401 || REJECTED_REFRESH_CODES.contains(&body.as_str()) =>
        {
            return Err(ProviderError::Auth(format!(
                "refresh token rejected ({status}): {body}"
            )));
        }
        Err(error) => return Err(error),
    };
    let mut refreshed = tokens.into_credentials(endpoints.provider, now)?;
    refreshed.refresh_token = refreshed
        .refresh_token
        .or_else(|| credentials.refresh_token.take());
    refreshed.chatgpt_account_id = refreshed
        .chatgpt_account_id
        .or_else(|| credentials.chatgpt_account_id.take());
    refreshed.email = refreshed.email.or_else(|| credentials.email.take());
    *credentials = refreshed;
    Ok(())
}

async fn post_token(
    client: &reqwest::Client,
    url: &Url,
    encoding: Encoding,
    params: &[(&str, &str)],
) -> Result<TokenResponse, ProviderError> {
    let request = client
        .post(url.clone())
        .header(ACCEPT, "application/json")
        .header(http::header::USER_AGENT, USER_AGENT)
        .timeout(REQUEST_TIMEOUT);
    let request = match encoding {
        Encoding::Form => request.form(params),
        Encoding::Json => request.json(&params.iter().copied().collect::<BTreeMap<_, _>>()),
    };
    let response = request.send().await.map_err(reqwest::Error::without_url)?;
    let status = response.status();
    if !status.is_success() {
        let body = read_response(response).await.ok();
        let description = body
            .as_deref()
            .and_then(|body| str::from_utf8(body).ok())
            .and_then(rejection_code);
        return Err(ProviderError::Status {
            status: status.as_u16(),
            body: description.unwrap_or("token request failed").to_owned(),
        });
    }
    let body = read_response(response).await?;
    serde_json::from_slice(&body).map_err(|error| {
        if error.is_data() {
            ProviderError::Other(format!(
                "invalid token response at line {} column {}",
                error.line(),
                error.column()
            ))
        } else {
            error.into()
        }
    })
}

/// Bound the small JSON responses from token and usage endpoints.
pub(crate) async fn read_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    const LIMIT: usize = 1024 * 1024;
    let oversized = || ProviderError::Other("provider response is too large".to_owned());
    if response
        .content_length()
        .is_some_and(|length| length > LIMIT as u64)
    {
        return Err(oversized());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        if chunk.len() > LIMIT - body.len() {
            return Err(oversized());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

const REJECTED_REFRESH_CODES: &[&str] = &[
    "invalid_grant",
    "refresh_token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
    "token_revoked",
    "invalid_client",
];

/// Only allow known OAuth rejection codes into diagnostics.
fn rejection_code(body: &str) -> Option<&'static str> {
    let json: Value = serde_json::from_str(body).ok()?;
    let code = match json.get("error")? {
        Value::String(code) => code.as_str(),
        error => error.get("code").or_else(|| error.get("type"))?.as_str()?,
    };
    REJECTED_REFRESH_CODES
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_known_oauth_rejection_codes() {
        for (body, expected) in [
            (r#"{"error":"INVALID_GRANT"}"#, Some("invalid_grant")),
            (
                r#"{"error":{"code":"refresh_token_reused"}}"#,
                Some("refresh_token_reused"),
            ),
            (
                r#"{"error":{"type":"invalid_grant"}}"#,
                Some("invalid_grant"),
            ),
            (r#"{"error":"unknown"}"#, None),
            ("oops", None),
        ] {
            assert_eq!(rejection_code(body), expected);
        }
    }

    #[test]
    fn token_lifetime_prefers_expires_in_then_jwt_then_fallback() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let response = |access: &str, expires_in: Option<i64>| TokenResponse {
            access_token: access.to_owned(),
            refresh_token: None,
            id_token: None,
            expires_in,
            account: None,
        };
        assert_eq!(
            response("x", Some(60)).expires_at(now),
            now + TimeDelta::seconds(60)
        );
        let jwt = jwt::tests::make_jwt(&serde_json::json!({"exp": 1_700_003_000}));
        assert_eq!(
            response(&jwt, None).expires_at(now),
            DateTime::from_timestamp(1_700_003_000, 0).unwrap()
        );
        assert_eq!(
            response("opaque", Some(i64::MAX)).expires_at(now),
            now + FALLBACK_LIFETIME
        );
    }
}
