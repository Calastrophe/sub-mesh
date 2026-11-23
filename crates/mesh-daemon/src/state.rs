//! Shared daemon state.

mod account;
mod admission;
mod owner;
mod settings;
mod status;
mod tokens;

#[cfg(test)]
pub mod tests;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use mesh_core::AccountId;
#[cfg(test)]
use mesh_providers::StoredAccount;
use mesh_providers::{ProviderError, TokenStore};
use mesh_relay::tailscale::LocalClient;
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::config::Settings;
use crate::poller;

pub use account::AccountState;
pub use settings::{RuntimeSettings, StateError};

use owner::Owner;
use status::{ReportStatus, TunnelStatus};

/// Everything the daemon shares between tasks.
pub struct AppState {
    pub settings: Settings,
    store: TokenStore,
    pub local: LocalClient,
    /// Client for upstream requests and token refreshes.
    pub upstream: reqwest::Client,
    runtime: Mutex<RuntimeSettings>,
    runtime_path: PathBuf,
    accounts: RwLock<BTreeMap<AccountId, Arc<AccountState>>>,
    token_store_access: Arc<tokio::sync::RwLock<()>>,
    tasks: Mutex<Option<JoinSet<()>>>,
    report_status: Mutex<ReportStatus>,
    tunnel_status: Mutex<TunnelStatus>,
    owner: RwLock<Owner>,
    changed: Notify,
}

impl AppState {
    pub fn new(
        mut settings: Settings,
        runtime: RuntimeSettings,
        upstream: reqwest::Client,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            store: TokenStore::new(settings.accounts_dir()),
            local: LocalClient::new(settings.tailscale_socket.clone()),
            runtime_path: settings.runtime_path(),
            upstream,
            runtime: Mutex::new(runtime),
            accounts: RwLock::new(BTreeMap::new()),
            token_store_access: Arc::new(tokio::sync::RwLock::new(())),
            tasks: Mutex::new(Some(JoinSet::new())),
            report_status: Mutex::new(ReportStatus::default()),
            tunnel_status: Mutex::new(TunnelStatus::default()),
            owner: RwLock::new(Owner::new(settings.initial_owner.take())),
            changed: Notify::new(),
            settings,
        })
    }

    pub fn account(&self, id: &AccountId) -> Option<Arc<AccountState>> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(id)
            .cloned()
    }

    /// All accounts, ordered by id.
    pub fn accounts(&self) -> Vec<Arc<AccountState>> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Add an account without starting its poller.
    #[cfg(test)]
    pub fn insert_account(&self, stored: StoredAccount) -> Arc<AccountState> {
        let account = AccountState::new(stored);
        self.accounts
            .write()
            .expect("accounts lock poisoned")
            .insert(account.id.clone(), account.clone());
        account
    }

    /// Re-read the token store: add new accounts (and start their pollers),
    /// drop removed ones, keep the windows of the rest.
    pub async fn reload(self: &Arc<Self>) -> Result<(), ProviderError> {
        let _access = self.token_store_access.write().await;
        let mut tasks = self.tasks.lock().expect("background tasks lock poisoned");
        let Some(tasks) = tasks.as_mut() else {
            return Ok(());
        };
        Self::reap_tasks(tasks);
        let on_disk = self.store.load_all()?;
        let disk_ids: HashSet<AccountId> = on_disk.iter().map(|a| a.id.clone()).collect();
        let mut added = Vec::new();
        {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            accounts.retain(|id, account| {
                let keep = disk_ids.contains(id);
                if !keep {
                    account.stop();
                }
                keep
            });
            for stored in on_disk {
                if let Some(account) = accounts.get(&stored.id)
                    && account.provider == stored.provider
                {
                    let mut inner = account.inner();
                    let login_changed = inner.stored.credentials.refresh_token
                        != stored.credentials.refresh_token
                        || inner.stored.credentials.access_token != stored.credentials.access_token;
                    inner.stored = stored;
                    if login_changed {
                        inner.auth_error = None;
                    }
                } else {
                    let account = AccountState::new(stored);
                    if let Some(previous) = accounts.insert(account.id.clone(), account.clone()) {
                        previous.stop();
                    }
                    added.push(account);
                }
            }
            self.prune_runtime(|id| {
                accounts.contains_key(id) || self.account_file(id).try_exists().unwrap_or(true)
            });
        }
        for account in added {
            tracing::info!(account = %account.id, provider = %account.provider, "account added");
            tasks.spawn(poller::run(self.clone(), account));
        }
        self.notify_changed();
        Ok(())
    }

    pub async fn delete_account(&self, id: &AccountId) -> Result<(), StateError> {
        let _access = self.token_store_access.write().await;
        if let Some(tasks) = self
            .tasks
            .lock()
            .expect("background tasks lock poisoned")
            .as_mut()
        {
            Self::reap_tasks(tasks);
        }
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        let account = accounts
            .get(id)
            .ok_or_else(|| StateError::NotFound(id.clone()))?;
        self.store.remove(id).map_err(StateError::Remove)?;
        account.stop();
        accounts.remove(id);
        self.prune_runtime(|other| other != id);
        self.notify_changed();
        tracing::info!(account = %id, "account removed");
        Ok(())
    }

    fn account_file(&self, id: &AccountId) -> PathBuf {
        self.store.dir().join(format!("{id}.json"))
    }

    pub async fn stop_background(&self) {
        let tasks = {
            let mut tasks = self.tasks.lock().expect("background tasks lock poisoned");
            for account in self.accounts() {
                account.stop();
            }
            tasks.take()
        };
        if let Some(mut tasks) = tasks {
            while let Some(result) = tasks.join_next().await {
                if let Err(err) = result {
                    tracing::warn!("background task failed: {err}");
                }
            }
        }
    }

    fn reap_tasks(tasks: &mut JoinSet<()>) {
        while let Some(result) = tasks.try_join_next() {
            if let Err(err) = result {
                tracing::warn!("background task failed: {err}");
            }
        }
    }
}
