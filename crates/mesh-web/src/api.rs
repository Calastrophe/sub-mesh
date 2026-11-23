//! Client for the management API. With `?mock` in the page URL, every call
//! is answered from [`crate::mock`] instead of the network.

use gloo_net::http::{Request, Response};
use mesh_core::api::{
    self, Direction, LogEntry, Overview, SetModels, SetReserve, Subscription, UsageReport,
};
use mesh_core::{AccountId, ApiError, ModelAllowlist, ReserveConfig};
use serde::de::DeserializeOwned;

use crate::mock;

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The request never got an answer.
    #[error("cannot reach the server: {0}")]
    Network(String),
    /// The server answered with an error status.
    #[error("{message} (HTTP {status})")]
    Api { status: u16, message: String },
    /// The answer was not what the API promises.
    #[error("unexpected response: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// `GET /api/v1/overview`.
pub async fn overview() -> Result<Overview> {
    if mock::enabled() {
        return Ok(mock::overview());
    }
    get_json(api::OVERVIEW_PATH).await
}

/// The path of a log request: `?limit=N`, with `&before=ID` to page back
/// from the entry with that id and `&direction=D` for one direction only.
fn logs_path(limit: u32, before: Option<i64>, direction: Option<Direction>) -> String {
    let mut path = format!("{}?limit={limit}", api::LOGS_PATH);
    if let Some(before) = before {
        path.push_str(&format!("&before={before}"));
    }
    if let Some(direction) = direction {
        path.push_str("&direction=");
        path.push_str(direction.as_str());
    }
    path
}

/// `GET /api/v1/logs`, see [`logs_path`].
pub async fn logs(
    limit: u32,
    before: Option<i64>,
    direction: Option<Direction>,
) -> Result<Vec<LogEntry>> {
    if mock::enabled() {
        return Ok(mock::logs(limit, before, direction));
    }
    get_json(&logs_path(limit, before, direction)).await
}

/// The path of a usage request: `?hours=N`.
fn usage_path(hours: u32) -> String {
    format!("{}?hours={hours}", api::USAGE_PATH)
}

/// `GET /api/v1/usage`, see [`usage_path`].
pub async fn usage(hours: u32) -> Result<UsageReport> {
    if mock::enabled() {
        return Ok(mock::usage(hours));
    }
    get_json(&usage_path(hours)).await
}

/// `PUT` a subscription's reserve; `None` goes back to the daemon default.
pub async fn set_reserve(id: &AccountId, reserve: Option<ReserveConfig>) -> Result<Subscription> {
    if mock::enabled() {
        return mock::set_reserve(id, reserve);
    }
    let request = Request::put(&api::reserve_path(id))
        .json(&SetReserve { reserve })
        .map_err(|error| Error::Decode(error.to_string()))?;
    decode(execute(request).await?).await
}

/// `PUT` a subscription's model allowlist; `None` goes back to the daemon
/// default.
pub async fn set_models(id: &AccountId, models: Option<ModelAllowlist>) -> Result<Subscription> {
    if mock::enabled() {
        return mock::set_models(id, models);
    }
    let request = Request::put(&api::models_path(id))
        .json(&SetModels { models })
        .map_err(|error| Error::Decode(error.to_string()))?;
    decode(execute(request).await?).await
}

/// Pause or resume a subscription.
pub async fn set_paused(id: &AccountId, paused: bool) -> Result<Subscription> {
    if mock::enabled() {
        return mock::set_paused(id, paused);
    }
    let path = if paused {
        api::pause_path(id)
    } else {
        api::resume_path(id)
    };
    let request = build(Request::post(&path))?;
    decode(execute(request).await?).await
}

/// `DELETE` a subscription and its saved tokens.
pub async fn remove(id: &AccountId) -> Result<()> {
    if mock::enabled() {
        return mock::remove(id);
    }
    let request = build(Request::delete(&api::subscription_path(id)))?;
    execute(request).await.map(drop)
}

fn build(builder: gloo_net::http::RequestBuilder) -> Result<Request> {
    builder
        .build()
        .map_err(|error| Error::Decode(error.to_string()))
}

async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T> {
    decode(execute(build(Request::get(path))?).await?).await
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    response
        .json()
        .await
        .map_err(|error| Error::Decode(error.to_string()))
}

/// Send `request`; a non-2xx status becomes [`Error::Api`] with the
/// server's [`ApiError`] message.
async fn execute(request: Request) -> Result<Response> {
    let response = request
        .send()
        .await
        .map_err(|error| Error::Network(error.to_string()))?;
    if response.ok() {
        return Ok(response);
    }
    let status = response.status();
    let reason = response.status_text();
    let message = match response.json::<ApiError>().await {
        Ok(body) => body.error,
        Err(_) if reason.is_empty() => "request failed".to_owned(),
        Err(_) => reason,
    };
    Err(Error::Api { status, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_paths_carry_only_the_set_parameters() {
        assert_eq!(logs_path(100, None, None), "/api/v1/logs?limit=100");
        assert_eq!(
            logs_path(50, Some(7), Some(Direction::Inbound)),
            "/api/v1/logs?limit=50&before=7&direction=inbound"
        );
        assert_eq!(
            logs_path(10, None, Some(Direction::Outbound)),
            "/api/v1/logs?limit=10&direction=outbound"
        );
    }

    #[test]
    fn usage_paths_carry_the_period() {
        assert_eq!(usage_path(168), "/api/v1/usage?hours=168");
    }
}
