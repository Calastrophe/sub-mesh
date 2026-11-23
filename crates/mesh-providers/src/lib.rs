//! Provider-specific logic for mesh: OAuth login, token refresh, auth header
//! replacement, rate-limit header parsing, and usage endpoint polling.
//!
//! [`refresh`] and [`poll_usage`] talk to the real providers. Their `_with`
//! forms take [`Endpoints`] so the daemon's configured endpoints and tests can
//! point them elsewhere.

use chrono::{DateTime, Utc};
use http::HeaderMap;
use mesh_core::{Provider, Window};
use url::Url;

mod auth;
mod claude;
mod codex;
mod credentials;
mod endpoints;
mod jwt;
mod login;
mod oauth;
mod secret;
mod store;
mod usage;

pub use credentials::{Credentials, StoredAccount};
pub use endpoints::Endpoints;
pub use login::LOGIN_TIMEOUT;
pub use secret::{read_or_create_secret, read_secret};
pub use store::{TokenStore, default_data_dir};
pub use usage::Usage;

/// Errors from provider operations.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A failed upstream request with a sanitized description.
    #[error("upstream returned {status}: {body}")]
    Status { status: u16, body: String },
    /// The refresh token was rejected; the owner must log in again.
    #[error("login is no longer valid: {0}")]
    Auth(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// Options for [`login`].
#[derive(Clone, Debug, Default)]
pub struct LoginOptions {
    /// Do not run a local callback server; ask the user to paste the
    /// redirect URL or code instead (for machines without a browser). A
    /// pasted URL or query must carry the matching OAuth state.
    pub manual: bool,
}

/// Run the interactive OAuth login for `provider`. Prints the URL to open on
/// stderr, waits for the callback (or a pasted code), exchanges the code, and
/// returns a new account with a fresh id. Does not save it.
///
/// The wait ends after [`LOGIN_TIMEOUT`].
pub async fn login(
    provider: Provider,
    client: &reqwest::Client,
    options: &LoginOptions,
) -> Result<StoredAccount, ProviderError> {
    login::run(client, options, &Endpoints::new(provider)).await
}

/// Refresh `credentials` in place. Returns [`ProviderError::Auth`] when the
/// refresh token is rejected.
pub async fn refresh(
    provider: Provider,
    client: &reqwest::Client,
    credentials: &mut Credentials,
) -> Result<(), ProviderError> {
    refresh_with(client, credentials, &Endpoints::new(provider)).await
}

/// [`refresh`] against `endpoints`.
pub async fn refresh_with(
    client: &reqwest::Client,
    credentials: &mut Credentials,
    endpoints: &Endpoints,
) -> Result<(), ProviderError> {
    oauth::refresh(client, endpoints, credentials).await
}

/// Base URL that consumer paths are relative to. A consumer request to
/// `/<provider>/<path>` goes upstream to `upstream_base(provider).join(path)`.
///
/// Codex is `https://chatgpt.com/backend-api/codex/`, so a consumer reaches
/// only the Codex model API (`responses`, `models`), never the account,
/// plugin, or settings routes elsewhere under `backend-api/`.
pub fn upstream_base(provider: Provider) -> Url {
    endpoints::upstream_base(provider).clone()
}

/// Join a consumer path and query (without the leading provider segment,
/// for example `v1/messages?beta=true`) onto [`upstream_base`].
///
/// Fails for paths that could leave the base: absolute URLs, `//host`,
/// backslashes, `.` or `..` segments (also percent-encoded), encoded path
/// separators, fragments, spaces, and control characters.
pub fn upstream_url(provider: Provider, path_and_query: &str) -> Result<Url, ProviderError> {
    endpoints::join_upstream(endpoints::upstream_base(provider), path_and_query)
}

/// Remove any consumer auth headers and insert this account's.
///
/// Claude: `Authorization: Bearer`, no `x-api-key`, and `oauth-2025-04-20`
/// added to `anthropic-beta`. Codex: `Authorization: Bearer` and
/// `ChatGPT-Account-ID`. Every other header is left as it is.
pub fn apply_auth(provider: Provider, credentials: &Credentials, headers: &mut HeaderMap) {
    match provider {
        Provider::Codex => codex::apply_auth(credentials, headers),
        Provider::Claude => claude::apply_auth(credentials, headers),
    }
}

/// Windows found in upstream response headers. Empty when there are none.
pub fn parse_rate_limit_headers(
    provider: Provider,
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Vec<Window> {
    match provider {
        Provider::Codex => codex::parse_rate_limit_headers(headers, now),
        Provider::Claude => claude::parse_rate_limit_headers(headers),
    }
}

/// Call the provider's usage endpoint once and return the raw JSON with the
/// windows read from it.
pub async fn poll_usage(
    provider: Provider,
    client: &reqwest::Client,
    credentials: &Credentials,
    now: DateTime<Utc>,
) -> Result<Usage, ProviderError> {
    poll_usage_with(client, credentials, &Endpoints::new(provider), now).await
}

/// [`poll_usage`] against `endpoints`.
pub async fn poll_usage_with(
    client: &reqwest::Client,
    credentials: &Credentials,
    endpoints: &Endpoints,
    now: DateTime<Utc>,
) -> Result<Usage, ProviderError> {
    usage::poll(client, endpoints, credentials, now).await
}

#[cfg(test)]
fn test_headers(pairs: &[(&str, &str)]) -> HeaderMap {
    pairs
        .iter()
        .map(|(name, value)| {
            (
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            )
        })
        .collect()
}
