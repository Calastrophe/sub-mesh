use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use mesh_core::{AccountId, Provider, Window};
use mesh_providers::StoredAccount;
use tokio::sync::watch;

/// The live state of one account.
pub struct AccountInner {
    pub stored: StoredAccount,
    pub windows: Vec<Window>,
    pub updated_at: DateTime<Utc>,
    pub auth_error: Option<String>,
    pub last_poll_error: Option<String>,
}

impl AccountInner {
    /// Merge fresh windows and mark the account as refreshed.
    pub fn merge(&mut self, windows: Vec<Window>, now: DateTime<Utc>) {
        merge_windows(&mut self.windows, windows);
        self.updated_at = now;
    }
}

/// One account, shared between the relay, the poller, and the admin API.
pub struct AccountState {
    pub id: AccountId,
    pub provider: Provider,
    /// Held while refreshing the token, so concurrent requests refresh once.
    pub(super) refresh: Arc<tokio::sync::Mutex<()>>,
    inner: Mutex<AccountInner>,
    stop: watch::Sender<bool>,
}

impl AccountState {
    pub(super) fn new(stored: StoredAccount) -> Arc<AccountState> {
        let (stop, _) = watch::channel(false);
        Arc::new(AccountState {
            id: stored.id.clone(),
            provider: stored.provider,
            refresh: Arc::new(tokio::sync::Mutex::new(())),
            inner: Mutex::new(AccountInner {
                stored,
                windows: Vec::new(),
                updated_at: Utc::now(),
                auth_error: None,
                last_poll_error: None,
            }),
            stop,
        })
    }

    /// Lock the account data. Never hold this across an `.await`.
    pub fn inner(&self) -> MutexGuard<'_, AccountInner> {
        self.inner.lock().expect("account lock poisoned")
    }

    /// A receiver that turns `true` once the account is removed.
    pub fn subscribe_stop(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    pub(super) fn stop(&self) {
        self.stop.send_replace(true);
    }
}

/// Newer windows replace older ones with the same id; the others are kept.
pub(super) fn merge_windows(current: &mut Vec<Window>, new: Vec<Window>) {
    for window in new {
        match current.iter_mut().find(|slot| slot.id == window.id) {
            Some(slot) => *slot = window,
            None => current.push(window),
        }
    }
}
