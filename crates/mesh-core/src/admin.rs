//! The daemon's loopback JSON API for `meshctl`. The default port is
//! [`crate::protocol::DEFAULT_ADMIN_PORT`].
//!
//! | Method   | Path                              | Body           | Returns       |
//! |----------|-----------------------------------|----------------|---------------|
//! | `GET`    | [`STATUS_PATH`]                   |                | [`AdminStatus`] |
//! | `POST`   | [`RELOAD_PATH`]                   |                | [`AdminStatus`] |
//! | `PUT`    | [`RESERVE_PATH`]                  | [`SetReserve`] | [`AdminStatus`] |
//! | `PUT`    | [`MODELS_PATH`]                   | [`SetModels`]  | [`AdminStatus`] |
//! | `POST`   | `/admin/v1/accounts/{id}/pause`   |                | [`AdminStatus`] |
//! | `POST`   | `/admin/v1/accounts/{id}/resume`  |                | [`AdminStatus`] |
//! | `DELETE` | `/admin/v1/accounts/{id}`         |                | [`AdminStatus`] |
//!
//! Every request must carry `Authorization: Bearer <token>`, where the token
//! is the content of [`TOKEN_FILE`] in the daemon's data directory. A missing
//! or wrong token is a `401` response.
//!
//! Errors are a non-2xx status with an [`crate::ApiError`] body.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::allowlist::ModelAllowlist;
use crate::provider::Provider;
use crate::report::AccountReport;
use crate::reserve::ReserveConfig;

/// Name of the file in the daemon's data directory that holds the admin token.
pub const TOKEN_FILE: &str = "admin-token";

/// `GET`: the daemon's full state.
pub const STATUS_PATH: &str = "/admin/v1/status";

/// `POST`: reload accounts from the token store (after `meshctl login`).
pub const RELOAD_PATH: &str = "/admin/v1/reload";

/// `PUT`: set the default reserve or one account's reserve.
pub const RESERVE_PATH: &str = "/admin/v1/reserve";

/// `PUT`: set or clear one account's model allowlist.
pub const MODELS_PATH: &str = "/admin/v1/models";

/// Path that pauses sharing for one account.
#[must_use]
pub fn pause_path(id: &AccountId) -> String {
    format!("/admin/v1/accounts/{id}/pause")
}

/// Path that resumes sharing for one account.
#[must_use]
pub fn resume_path(id: &AccountId) -> String {
    format!("/admin/v1/accounts/{id}/resume")
}

/// Path that removes one account (`DELETE`), including its saved tokens.
#[must_use]
pub fn account_path(id: &AccountId) -> String {
    format!("/admin/v1/accounts/{id}")
}

/// The daemon's full state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminStatus {
    /// Stable name of the daemon's machine.
    pub node: String,
    /// Base URL of the mesh server receiving the daemon's reports.
    pub server_url: String,
    /// When the current tunnel opened, if connected.
    pub tunnel_connected_since: Option<DateTime<Utc>>,
    /// Error from the last failed tunnel attempt while disconnected.
    pub tunnel_error: Option<String>,
    /// The reserve for accounts without their own.
    pub default_reserve: ReserveConfig,
    /// The model allowlist from `daemon.toml` for accounts of each provider
    /// without their own. A provider that is missing allows every model.
    #[serde(default)]
    pub default_models: Vec<(Provider, ModelAllowlist)>,
    /// When the server last accepted a report from this daemon.
    pub last_report_ok: Option<DateTime<Utc>>,
    /// Error from the most recent report attempt, if it failed.
    pub last_report_error: Option<String>,
    pub accounts: Vec<AdminAccount>,
}

/// One account as the admin API shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminAccount {
    /// What the daemon reports to the server for this account.
    pub report: AccountReport,
    pub paused: bool,
    /// The account's own reserve, when set; otherwise the default applies.
    pub reserve: Option<ReserveConfig>,
    /// The account's own model allowlist, when set; otherwise the default
    /// for its provider applies.
    #[serde(default)]
    pub models: Option<ModelAllowlist>,
    /// Set when the login is broken and needs `meshctl login` again.
    pub auth_error: Option<String>,
    /// Error from the most recent usage poll, if it failed.
    pub last_poll_error: Option<String>,
}

/// Body of `PUT` [`RESERVE_PATH`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetReserve {
    /// The account to change. `None` changes the default.
    pub account: Option<AccountId>,
    /// The new reserve. `None` clears an account's own reserve so the
    /// default applies again (not allowed when `account` is `None`).
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub reserve: Option<ReserveConfig>,
}

/// Body of `PUT` [`MODELS_PATH`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModels {
    /// The account to change.
    pub account: AccountId,
    /// The account's own allowlist. `None` clears it so the default for its
    /// provider applies again.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub models: Option<ModelAllowlist>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_require_explicit_null_to_clear() {
        fn check<T: serde::de::DeserializeOwned>(clear: &str, missing: &str, unknown: &str) {
            assert!(serde_json::from_str::<T>(clear).is_ok());
            assert!(serde_json::from_str::<T>(missing).is_err());
            assert!(serde_json::from_str::<T>(unknown).is_err());
        }

        check::<SetReserve>(
            r#"{"account":"id","reserve":null}"#,
            r#"{"account":"id"}"#,
            r#"{"acount":"id","reserve":null}"#,
        );
        check::<SetModels>(
            r#"{"account":"id","models":null}"#,
            r#"{"account":"id"}"#,
            r#"{"account":"id","models":null,"extra":true}"#,
        );
        check::<crate::api::SetReserve>(
            r#"{"reserve":null}"#,
            "{}",
            r#"{"reserve":null,"extra":true}"#,
        );
        check::<crate::api::SetModels>(
            r#"{"models":null}"#,
            "{}",
            r#"{"models":null,"extra":true}"#,
        );
    }
}
