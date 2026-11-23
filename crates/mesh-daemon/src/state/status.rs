use chrono::{DateTime, Utc};
use mesh_core::admin::{AdminAccount, AdminStatus};
use mesh_core::{AccountReport, DaemonReport};

use super::{AccountState, AppState, RuntimeSettings};

/// Result of the last report to the server.
#[derive(Debug, Default)]
pub(super) struct ReportStatus {
    last_ok: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

/// State of the tunnel to the server.
#[derive(Debug, Default)]
pub(super) struct TunnelStatus {
    connected_since: Option<DateTime<Utc>>,
    error: Option<String>,
}

impl AppState {
    /// Record that the tunnel is up and ask for a prompt report, so the
    /// server sees fresh state as soon as it can reach the daemon.
    pub fn record_tunnel_up(&self) {
        {
            let mut status = self.tunnel_status.lock().expect("tunnel status poisoned");
            status.connected_since = Some(Utc::now());
            status.error = None;
        }
        self.notify_changed();
    }

    /// Record that the tunnel is down, and why.
    pub fn record_tunnel_down(&self, error: String) {
        let mut status = self.tunnel_status.lock().expect("tunnel status poisoned");
        status.connected_since = None;
        status.error = Some(error);
    }

    /// Ask the reporter to send a report soon.
    pub fn notify_changed(&self) {
        self.changed.notify_one();
    }

    /// Resolves when something asked for a prompt report.
    pub async fn wait_changed(&self) {
        self.changed.notified().await;
    }

    pub(super) fn account_view(
        &self,
        account: &AccountState,
        runtime: &RuntimeSettings,
        now: DateTime<Utc>,
    ) -> AdminAccount {
        let paused = runtime.paused(&account.id);
        let reserve_override = runtime
            .accounts
            .get(&account.id)
            .and_then(|settings| settings.reserve.clone());
        let reserve = runtime.reserve_for(&account.id, &self.settings.reserve);
        let models = runtime.models_for(&account.id, account.provider, &self.settings.models);
        let inner = account.inner();
        AdminAccount {
            report: AccountReport {
                id: account.id.clone(),
                provider: account.provider,
                label: inner
                    .stored
                    .label
                    .clone()
                    .or_else(|| inner.stored.credentials.email.clone()),
                windows: inner.windows.clone(),
                admission: inner.admission(paused, &reserve, now),
                paused,
                reserve,
                models,
                updated_at: inner.updated_at,
            },
            paused,
            reserve: reserve_override,
            models: runtime.own_models(&account.id).cloned(),
            auth_error: inner.auth_error.clone(),
            last_poll_error: inner.last_poll_error.clone(),
        }
    }

    /// The full state for the admin API.
    pub fn admin_status(&self, now: DateTime<Utc>) -> AdminStatus {
        let (last_report_ok, last_report_error) = {
            let status = self.report_status.lock().expect("report status poisoned");
            (status.last_ok, status.last_error.clone())
        };
        let (tunnel_connected_since, tunnel_error) = {
            let status = self.tunnel_status.lock().expect("tunnel status poisoned");
            (status.connected_since, status.error.clone())
        };
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let runtime = self.runtime();
        AdminStatus {
            node: self.settings.node.clone(),
            server_url: self.settings.server_url.clone(),
            tunnel_connected_since,
            tunnel_error,
            default_reserve: runtime
                .default_reserve
                .clone()
                .unwrap_or_else(|| self.settings.reserve.clone()),
            default_models: self
                .settings
                .models
                .iter()
                .map(|(provider, models)| (*provider, models.clone()))
                .collect(),
            last_report_ok,
            last_report_error,
            accounts: accounts
                .values()
                .map(|account| self.account_view(account, &runtime, now))
                .collect(),
        }
    }

    /// The report sent to the server.
    pub fn daemon_report(&self) -> DaemonReport {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let runtime = self.runtime();
        let now = Utc::now();
        DaemonReport {
            node: self.settings.node.clone(),
            owner: self.owner(),
            accounts: accounts
                .values()
                .map(|account| self.account_view(account, &runtime, now).report)
                .collect(),
            sent_at: now,
        }
    }

    /// Record the outcome of a report.
    pub fn record_report(&self, result: Result<(), String>) {
        let mut status = self.report_status.lock().expect("report status poisoned");
        match result {
            Ok(()) => {
                status.last_ok = Some(Utc::now());
                status.last_error = None;
            }
            Err(message) => status.last_error = Some(message),
        }
    }
}
