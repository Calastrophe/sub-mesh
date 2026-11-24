//! The server's JSON management API for `mesh-web`. Tailnet whois identifies
//! callers. Only a subscription's owner may change it. Each caller sees in
//! the log only their own requests and the requests that others made with
//! the caller's subscriptions.
//!
//! | Method   | Path                                   | Body           | Returns            |
//! |----------|----------------------------------------|----------------|--------------------|
//! | `GET`    | `/api/v1/overview`                     |                | [`Overview`]       |
//! | `GET`    | `/api/v1/logs?limit=N&before=ID&direction=D` |          | [`Vec<LogEntry>`]  |
//! | `GET`    | `/api/v1/usage?hours=N`                |                | [`UsageReport`]    |
//! | `PUT`    | `/api/v1/subscriptions/{id}/reserve`   | [`SetReserve`] | [`Subscription`]   |
//! | `PUT`    | `/api/v1/subscriptions/{id}/models`    | [`SetModels`]  | [`Subscription`]   |
//! | `POST`   | `/api/v1/subscriptions/{id}/pause`     |                | [`Subscription`]   |
//! | `POST`   | `/api/v1/subscriptions/{id}/resume`    |                | [`Subscription`]   |
//! | `DELETE` | `/api/v1/subscriptions/{id}`           |                | `204 No Content`   |
//!
//! Errors are a non-2xx status with an [`crate::ApiError`] body: `403` when the
//! caller does not own the subscription, `404` for an unknown one, and `502`
//! when the owning daemon cannot be reached.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::admission::Admission;
use crate::allowlist::ModelAllowlist;
use crate::provider::Provider;
use crate::reserve::ReserveConfig;
use crate::window::Window;

mod settings;

pub use settings::*;

/// Path prefix of the management API.
pub const PREFIX: &str = "/api/v1";

/// `GET`: everything the dashboard shows.
pub const OVERVIEW_PATH: &str = "/api/v1/overview";

/// `GET`: the caller's log, newest first: their own requests and the
/// requests others made with their subscriptions. Takes `?limit=N`,
/// `?before=ID` to page back from the entry with that id, and
/// `?direction=outbound` or `?direction=inbound` for only one of the two.
/// The server keeps the log for [`LOG_RETENTION_DAYS`] days, so older
/// entries are gone.
pub const LOGS_PATH: &str = "/api/v1/logs";

/// `GET`: what the mesh used of the caller's own subscriptions, per user and
/// model, from the request log. Takes `?hours=N`, from 1 to
/// [`MAX_USAGE_HOURS`]; the default is [`DEFAULT_USAGE_HOURS`]. Only the
/// owner of a subscription sees its usage. It sums the request log, which
/// the server keeps for [`LOG_RETENTION_DAYS`] days.
pub const USAGE_PATH: &str = "/api/v1/usage";

/// How many days the server keeps request log rows. It deletes older rows
/// at start and then every hour.
pub const LOG_RETENTION_DAYS: u32 = 14;

/// The period [`USAGE_PATH`] covers without `hours`: one week.
pub const DEFAULT_USAGE_HOURS: u32 = 168;

/// The longest period [`USAGE_PATH`] covers: the whole log, which is
/// [`LOG_RETENTION_DAYS`] days.
pub const MAX_USAGE_HOURS: u32 = LOG_RETENTION_DAYS * 24;

/// Path that sets a subscription's reserve (`PUT`).
#[must_use]
pub fn reserve_path(id: &AccountId) -> String {
    format!("{PREFIX}/subscriptions/{id}/reserve")
}

/// Path that sets a subscription's model allowlist (`PUT`).
#[must_use]
pub fn models_path(id: &AccountId) -> String {
    format!("{PREFIX}/subscriptions/{id}/models")
}

/// Path that pauses a subscription (`POST`).
#[must_use]
pub fn pause_path(id: &AccountId) -> String {
    format!("{PREFIX}/subscriptions/{id}/pause")
}

/// Path that resumes a subscription (`POST`).
#[must_use]
pub fn resume_path(id: &AccountId) -> String {
    format!("{PREFIX}/subscriptions/{id}/resume")
}

/// Path that removes a subscription and its saved tokens (`DELETE`).
#[must_use]
pub fn subscription_path(id: &AccountId) -> String {
    format!("{PREFIX}/subscriptions/{id}")
}

/// The caller, as the server identified it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// Tailnet login, or `None` for development callers without whois.
    pub login: Option<String>,
    /// The caller's node name.
    pub node: String,
}

/// Body of `GET` [`OVERVIEW_PATH`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    /// Server time, so the dashboard can show "resets in" consistently.
    pub now: DateTime<Utc>,
    pub me: Me,
    pub subscriptions: Vec<Subscription>,
    pub daemons: Vec<DaemonInfo>,
    /// Users who own subscriptions or made requests in the last hour.
    pub users: Vec<UserActivity>,
}

/// One subscription (a logged-in account on some daemon).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub id: AccountId,
    pub provider: Provider,
    /// Human-readable label, usually the account email.
    pub label: Option<String>,
    /// Tailnet login of the owning daemon's machine, when known.
    pub owner: Option<String>,
    /// Node name of the owning daemon.
    pub node: String,
    /// Whether the owning daemon reported recently.
    pub online: bool,
    pub paused: bool,
    /// The reserve in effect for this subscription.
    pub reserve: ReserveConfig,
    /// The model allowlist in effect. `None` allows every model.
    pub models: Option<ModelAllowlist>,
    /// Model ids the provider last listed for this subscription, before the
    /// allowlist. Empty until a client has fetched the model list.
    pub available_models: Vec<String>,
    pub windows: Vec<Window>,
    /// The daemon's decision with server-side overrides such as a cooldown.
    pub admission: Admission,
    /// When the server's active cooldown ends.
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Requests currently being relayed through this subscription.
    pub in_flight: u32,
    /// Consumer requests served in the last hour.
    pub requests_last_hour: u64,
    /// Higher scores are preferred for new sessions. `None` means ineligible.
    pub score: Option<f64>,
    /// True when the caller owns this subscription and may change it.
    pub can_manage: bool,
    /// When the windows were last refreshed by the daemon.
    pub updated_at: DateTime<Utc>,
}

/// One daemon known to the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub node: String,
    pub owner: Option<String>,
    /// When the daemon's current tunnel was opened, if it is connected.
    pub connected_since: Option<DateTime<Utc>>,
    pub last_seen: DateTime<Utc>,
    pub online: bool,
}

/// Request activity of one tailnet user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserActivity {
    pub login: String,
    /// Subscriptions this user owns.
    pub subscriptions: u32,
    /// Consumer requests this user made in the last hour.
    pub requests_last_hour: u64,
}

/// How a consumer request reached the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Http,
    Websocket,
}

/// Which way a logged request went, seen from the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The caller made the request, served by any subscription.
    Outbound,
    /// Someone else made the request, served by one of the caller's
    /// subscriptions.
    Inbound,
}

impl Direction {
    /// The value of the `direction` query parameter.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Outbound => "outbound",
            Direction::Inbound => "inbound",
        }
    }
}

/// The tokens that responses used, as the provider reported them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// All input tokens, including those read from or written to the prompt
    /// cache.
    pub input: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read: u64,
    /// Input tokens written to the prompt cache. Only Claude reports them.
    pub cache_write: u64,
    /// All output tokens, including reasoning tokens.
    pub output: u64,
    /// Output tokens spent on reasoning. Only Codex reports them.
    pub reasoning: u64,
}

/// One consumer request from the server's log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Increases with each request. Pass it as `before` to page back.
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub direction: Direction,
    /// The caller's tailnet login, when known.
    pub login: Option<String>,
    /// The caller's node name.
    pub consumer: String,
    pub provider: Provider,
    pub transport: Transport,
    pub method: String,
    /// The path below the provider's prefix, without the query, such as
    /// `responses` or `v1/messages`.
    pub path: String,
    /// The model the request asked for, when its body or first message said.
    pub model: Option<String>,
    /// The client, from its `user-agent`.
    pub client: Option<String>,
    /// The conversation, when the request named one.
    pub session: Option<String>,
    /// The subscription that served it, if any was tried.
    pub account_id: Option<AccountId>,
    /// The tailnet login that owns the serving subscription, when known.
    pub owner: Option<String>,
    /// The node of the daemon that served it.
    pub node: Option<String>,
    /// HTTP status returned to the consumer: `101` for an opened WebSocket.
    pub status: u16,
    /// What went wrong, when mesh refused the request or the provider
    /// answered with an error.
    pub error: Option<String>,
    /// Time until the response headers, or until the WebSocket opened.
    pub latency_ms: u64,
    /// Time until the response body ended or the WebSocket closed. `None`
    /// while that has not happened.
    pub duration_ms: Option<u64>,
    /// How many accounts were tried.
    pub attempts: u32,
    /// How many model responses the request carried: one for HTTP, one per
    /// completed response on a WebSocket.
    pub responses: u32,
    /// The tokens of those responses, summed. `None` when the provider
    /// reported none.
    pub usage: Option<TokenUsage>,
}

/// What the mesh used of the caller's subscriptions in a period.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    /// The start of the period. It ends now.
    pub since: DateTime<Utc>,
    /// One row per subscription, user and model, largest spend first.
    pub rows: Vec<UsageRow>,
}

/// The requests that one user made with one of the caller's subscriptions
/// and one model. The caller's own requests count as one user too.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub account_id: AccountId,
    pub provider: Provider,
    /// The user's tailnet login. `None` for requests made without one,
    /// which only happens in development.
    pub login: Option<String>,
    /// The model the requests asked for, when they said.
    pub model: Option<String>,
    pub requests: u64,
    /// Model responses that reported usage.
    pub responses: u64,
    pub tokens: TokenUsage,
    /// The price-weighted tokens, from [`crate::spend::spend`]. Compare
    /// spends only within one provider.
    pub spend: f64,
    pub last_used: DateTime<Utc>,
}

/// Reserve update sent to [`reserve_path`] and forwarded to the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetReserve {
    /// The subscription's own reserve. `None` clears it so the daemon
    /// default applies again.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub reserve: Option<ReserveConfig>,
}

/// Model allowlist update sent to [`models_path`] and forwarded to the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModels {
    /// The subscription's own allowlist. `None` clears it so the daemon
    /// default for its provider applies again.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub models: Option<ModelAllowlist>,
}
