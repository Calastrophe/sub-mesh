//! Calls to the providers' usage endpoints.

use chrono::{DateTime, Utc};
use http::header::{ACCEPT, USER_AGENT};
use http::{HeaderMap, HeaderValue};
use mesh_core::{Provider, Window};
use serde::Serialize;

use crate::oauth::{REQUEST_TIMEOUT, USER_AGENT as MESH_USER_AGENT, read_response};
use crate::{Credentials, Endpoints, ProviderError, apply_auth, claude, codex};

/// A provider's usage response and the windows read from it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Usage {
    /// The endpoint's JSON body as received.
    pub raw: serde_json::Value,
    pub windows: Vec<Window>,
}

/// GET the usage endpoint and return its JSON body.
async fn fetch(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    credentials: &Credentials,
) -> Result<serde_json::Value, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(USER_AGENT, HeaderValue::from_static(MESH_USER_AGENT));
    apply_auth(endpoints.provider, credentials, &mut headers);

    let response = client
        .get(endpoints.usage_url.clone())
        .headers(headers)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderError::Status {
            status: status.as_u16(),
            body: "usage request failed".to_owned(),
        });
    }
    let body = read_response(response).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// GET the usage endpoint and read its windows.
pub(crate) async fn poll(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    credentials: &Credentials,
    now: DateTime<Utc>,
) -> Result<Usage, ProviderError> {
    let raw = fetch(client, endpoints, credentials).await?;
    let windows = match endpoints.provider {
        Provider::Codex => codex::parse_usage(&raw, now),
        Provider::Claude => claude::parse_usage(&raw),
    };
    Ok(Usage { raw, windows })
}

pub(crate) fn window_from_percent(
    id: String,
    duration_minutes: Option<u32>,
    used_percent: f64,
    resets_at: Option<DateTime<Utc>>,
) -> Window {
    Window {
        id,
        duration_minutes,
        used: (used_percent / 100.0).clamp(0.0, 1.0),
        resets_at,
        limited: used_percent >= 100.0,
    }
}
