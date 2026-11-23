use std::fmt;

use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::{AccountId, Provider};
use serde::{Deserialize, Serialize};

/// OAuth credentials for one account.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// ChatGPT account id, sent as `ChatGPT-Account-ID` (Codex only).
    pub chatgpt_account_id: Option<String>,
    pub email: Option<String>,
}

impl Credentials {
    /// True when the access token expires within 5 minutes of `now`.
    /// An unknown expiry never needs a refresh.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| {
            expires_at.signed_duration_since(now) <= TimeDelta::minutes(5)
        })
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("chatgpt_account_id", &self.chatgpt_account_id)
            .field("email", &self.email)
            .finish()
    }
}

/// An account as saved on disk by the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredAccount {
    pub id: AccountId,
    pub provider: Provider,
    pub label: Option<String>,
    pub credentials: Credentials,
}
