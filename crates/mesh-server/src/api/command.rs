use std::time::Duration;

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use mesh_core::api::{SetModels, SetReserve};
use mesh_core::protocol::{CONTROL_PREFIX, ON_BEHALF_HEADER};
use mesh_core::{AccountReport, ApiError};

use super::failure::ApiFailure;
use crate::AppState;
use crate::registry::ControlTarget;

const MAX_REPLY_BYTES: usize = 2 * 1024 * 1024;

/// How long one control call may take in total. The daemon answers from
/// memory, so a longer wait means it is stuck or unreachable.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// A change to one account, sent to the daemon that holds it over its tunnel.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SetReserve(SetReserve),
    SetModels(SetModels),
    Pause,
    Resume,
    Delete,
}

/// What the daemon answered.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The account's new state.
    Report(Box<AccountReport>),
    /// The account is gone.
    Removed,
}

impl Command {
    fn method(&self) -> Method {
        match self {
            Command::SetReserve(_) | Command::SetModels(_) => Method::PUT,
            Command::Pause | Command::Resume => Method::POST,
            Command::Delete => Method::DELETE,
        }
    }

    /// The end of the control path, after the account id.
    fn suffix(&self) -> &'static str {
        match self {
            Command::SetReserve(_) => "/reserve",
            Command::SetModels(_) => "/models",
            Command::Pause => "/pause",
            Command::Resume => "/resume",
            Command::Delete => "",
        }
    }

    /// The control path and query for `target`.
    fn path(&self, target: &ControlTarget) -> String {
        format!("{CONTROL_PREFIX}/accounts/{}{}", target.id, self.suffix())
    }

    /// Send the command to the daemon that holds `target`, over its tunnel,
    /// acting for `on_behalf_of` (the daemon checks that this login owns it).
    ///
    /// The whole call, answer included, must finish within
    /// [`CONTROL_TIMEOUT`]. A daemon that has no open tunnel, fails, or does
    /// not answer in time is a `502`. A `4xx` answer passes through with its
    /// status. Any other failure is also a `502`.
    pub async fn send(
        &self,
        state: &AppState,
        target: &ControlTarget,
        on_behalf_of: Option<&str>,
    ) -> Result<Outcome, ApiFailure> {
        let unreachable =
            || ApiFailure::bad_gateway(format!("cannot reach daemon {}", target.node));
        let client = target
            .generation
            .and_then(|generation| state.tunnel_client(&target.node, generation))
            .ok_or_else(|| {
                tracing::warn!(node = %target.node, "daemon has no open tunnel for a control call");
                unreachable()
            })?;
        let mut headers = HeaderMap::new();
        if let Some(login) = on_behalf_of {
            let value = HeaderValue::from_str(login)
                .map_err(|_| ApiFailure::internal("the caller's login is not a valid header"))?;
            headers.insert(ON_BEHALF_HEADER, value);
        }
        let mut body = Bytes::new();
        let json = match self {
            Command::SetReserve(reserve) => Some(serde_json::to_vec(reserve)),
            Command::SetModels(models) => Some(serde_json::to_vec(models)),
            Command::Pause | Command::Resume | Command::Delete => None,
        };
        if let Some(json) = json {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            body = json
                .map_err(|_| ApiFailure::internal("cannot encode the command"))?
                .into();
        }
        let exchange = async {
            let response = client
                .send(self.method(), &self.path(target), headers, body)
                .await
                .map_err(|error| {
                    tracing::warn!(node = %target.node, %error, "cannot reach daemon for control call");
                    unreachable()
                })?;
            self.answer(response, target).await
        };
        tokio::time::timeout(CONTROL_TIMEOUT, exchange)
            .await
            .map_err(|_| {
                tracing::warn!(node = %target.node, "daemon control call timed out");
                ApiFailure::bad_gateway(format!("daemon {} did not answer in time", target.node))
            })?
    }

    /// Check the daemon's answer and turn it into an [`Outcome`].
    async fn answer(
        &self,
        response: Response,
        target: &ControlTarget,
    ) -> Result<Outcome, ApiFailure> {
        let status = response.status();
        if matches!(self, Command::Delete) && status == StatusCode::NO_CONTENT {
            return Ok(Outcome::Removed);
        }
        let body = read_reply(response.into_body(), target).await;
        if status.is_client_error() {
            let message =
                body.map_or_else(|_| status.to_string(), |body| daemon_message(&body, status));
            return Err(ApiFailure::new(status, message));
        }
        let body = body?;
        if status.is_success() {
            return self.outcome(&body, target);
        }
        let message = daemon_message(&body, status);
        tracing::warn!(node = %target.node, %status, %message, "daemon control call failed");
        Err(ApiFailure::bad_gateway(format!(
            "daemon {} answered {status}: {message}",
            target.node
        )))
    }

    fn outcome(&self, body: &[u8], target: &ControlTarget) -> Result<Outcome, ApiFailure> {
        if matches!(self, Command::Delete) {
            return Err(ApiFailure::bad_gateway(format!(
                "daemon {} did not confirm deletion",
                target.node
            )));
        }
        let report: AccountReport = serde_json::from_slice(body).map_err(|error| {
            tracing::warn!(node = %target.node, %error, "daemon sent an unreadable account");
            ApiFailure::bad_gateway(format!("daemon {} sent an invalid answer", target.node))
        })?;
        if report.id != target.id {
            return Err(ApiFailure::bad_gateway(format!(
                "daemon {} answered for a different account",
                target.node
            )));
        }
        if report.provider != target.provider {
            return Err(ApiFailure::bad_gateway(format!(
                "daemon {} answered for a different provider",
                target.node
            )));
        }
        Ok(Outcome::Report(Box::new(report)))
    }
}

async fn read_reply(body: Body, target: &ControlTarget) -> Result<BytesMut, ApiFailure> {
    let mut chunks = body.into_data_stream();
    let mut reply = BytesMut::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!(node = %target.node, %error, "cannot read daemon control reply");
            ApiFailure::bad_gateway(format!("cannot read answer from daemon {}", target.node))
        })?;
        if chunk.len() > MAX_REPLY_BYTES - reply.len() {
            return Err(ApiFailure::bad_gateway(format!(
                "daemon {} sent an oversized answer",
                target.node
            )));
        }
        reply.extend_from_slice(&chunk);
    }
    Ok(reply)
}

/// The message in a daemon's [`ApiError`] body, or its text, or the status.
fn daemon_message(body: &[u8], status: StatusCode) -> String {
    if let Ok(error) = serde_json::from_slice::<ApiError>(body) {
        return error.error;
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        status.to_string()
    } else {
        text.chars().take(200).collect()
    }
}
