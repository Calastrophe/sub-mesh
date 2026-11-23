use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::admission::Admission;
use crate::allowlist::ModelAllowlist;
use crate::provider::Provider;
use crate::reserve::ReserveConfig;
use crate::window::Window;

/// The state of one account as a daemon reports it to the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountReport {
    pub id: AccountId,
    pub provider: Provider,
    /// Human-readable label, such as the account email.
    pub label: Option<String>,
    /// The most recently observed usage windows.
    pub windows: Vec<Window>,
    pub admission: Admission,
    /// Whether the owner paused sharing for this account.
    #[serde(default)]
    pub paused: bool,
    /// Effective reserve, using the daemon default when there is no override.
    #[serde(default)]
    pub reserve: ReserveConfig,
    /// Effective model allowlist: the account's own, else the daemon's
    /// default for the provider. `None` allows every model.
    #[serde(default)]
    pub models: Option<ModelAllowlist>,
    /// When `windows` were last refreshed from the provider.
    pub updated_at: DateTime<Utc>,
}

/// Periodic report from a daemon to `POST` [`crate::protocol::REPORT_PATH`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonReport {
    /// Stable name of the daemon's machine, usually its tailnet host name.
    pub node: String,
    /// The tailnet login of the daemon's machine, as the daemon sees it.
    /// The server prefers its own whois answer and only falls back to this
    /// when whois is disabled for development.
    #[serde(default)]
    pub owner: Option<String>,
    pub accounts: Vec<AccountReport>,
    pub sent_at: DateTime<Utc>,
}
