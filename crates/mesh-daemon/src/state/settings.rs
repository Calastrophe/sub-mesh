use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::MutexGuard;

use chrono::Utc;
use http::StatusCode;
use mesh_core::{AccountId, AccountReport, ModelAllowlist, Provider, ReserveConfig, model_allowed};
use mesh_providers::ProviderError;
use serde::{Deserialize, Serialize};

use super::{AccountState, AppState};

/// Settings the owner changes while the daemon runs. Saved to
/// `daemon-state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeSettings {
    /// Overrides `reserve` from the config file.
    pub default_reserve: Option<ReserveConfig>,
    pub accounts: BTreeMap<AccountId, AccountSettings>,
}

/// Per-account runtime settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSettings {
    pub paused: bool,
    pub reserve: Option<ReserveConfig>,
    /// Overrides the `[models]` default for the account's provider.
    pub models: Option<ModelAllowlist>,
}

impl RuntimeSettings {
    /// Load the file. A missing file gives the defaults.
    pub fn load(path: &Path) -> io::Result<RuntimeSettings> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(RuntimeSettings::default()),
            Err(err) => Err(err),
        }
    }

    /// Write the file atomically (temp file, then rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    }

    pub(super) fn paused(&self, id: &AccountId) -> bool {
        self.accounts.get(id).is_some_and(|account| account.paused)
    }

    pub(super) fn reserve_for(
        &self,
        id: &AccountId,
        config_default: &ReserveConfig,
    ) -> ReserveConfig {
        self.accounts
            .get(id)
            .and_then(|account| account.reserve.clone())
            .or_else(|| self.default_reserve.clone())
            .unwrap_or_else(|| config_default.clone())
    }

    pub(super) fn own_models(&self, id: &AccountId) -> Option<&ModelAllowlist> {
        self.accounts
            .get(id)
            .and_then(|account| account.models.as_ref())
    }

    /// The allowlist in effect: the account's own, else the config default
    /// for its provider, else `None` (every model).
    pub(super) fn models_for(
        &self,
        id: &AccountId,
        provider: Provider,
        config_default: &BTreeMap<Provider, ModelAllowlist>,
    ) -> Option<ModelAllowlist> {
        self.own_models(id)
            .or_else(|| config_default.get(&provider))
            .cloned()
    }
}

/// Why a state change failed.
#[derive(Debug)]
pub enum StateError {
    NotFound(AccountId),
    Invalid(String),
    Persist(io::Error),
    Remove(ProviderError),
}

impl StateError {
    /// The HTTP status the admin and control APIs answer with.
    pub fn status(&self) -> StatusCode {
        match self {
            StateError::NotFound(_) => StatusCode::NOT_FOUND,
            StateError::Invalid(_) => StatusCode::BAD_REQUEST,
            StateError::Persist(_) | StateError::Remove(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::NotFound(id) => write!(f, "no account {id}"),
            StateError::Invalid(message) => f.write_str(message),
            StateError::Persist(err) => write!(f, "cannot save settings: {err}"),
            StateError::Remove(err) => write!(f, "cannot remove account: {err}"),
        }
    }
}

impl std::error::Error for StateError {}

impl AppState {
    pub(super) fn runtime(&self) -> MutexGuard<'_, RuntimeSettings> {
        self.runtime.lock().expect("runtime settings lock poisoned")
    }

    pub(super) fn paused_and_reserve(&self, id: &AccountId) -> (bool, ReserveConfig) {
        let runtime = self.runtime();
        (
            runtime.paused(id),
            runtime.reserve_for(id, &self.settings.reserve),
        )
    }

    /// Pause or resume sharing for one account.
    pub fn set_paused(&self, id: &AccountId, paused: bool) -> Result<AccountReport, StateError> {
        self.update_account_settings(id, |settings| settings.paused = paused)
    }

    pub fn set_default_reserve(&self, reserve: Option<ReserveConfig>) -> Result<(), StateError> {
        let reserve = reserve.ok_or_else(|| {
            StateError::Invalid("the default reserve cannot be cleared".to_owned())
        })?;
        validate_reserve(&reserve)?;
        let mut runtime = self.runtime();
        self.update_runtime(&mut runtime, |updated| {
            updated.default_reserve = Some(reserve)
        })
    }

    pub fn set_account_reserve(
        &self,
        id: &AccountId,
        reserve: Option<ReserveConfig>,
    ) -> Result<AccountReport, StateError> {
        if let Some(reserve) = &reserve {
            validate_reserve(reserve)?;
        }
        self.update_account_settings(id, |settings| settings.reserve = reserve)
    }

    /// Set or clear one account's own model allowlist. Clearing makes the
    /// config default for its provider apply again.
    pub fn set_account_models(
        &self,
        id: &AccountId,
        models: Option<ModelAllowlist>,
    ) -> Result<AccountReport, StateError> {
        self.update_account_settings(id, |settings| settings.models = models)
    }

    /// The model allowlist in effect for an account, where `None` allows
    /// every model.
    pub fn effective_models(&self, account: &AccountState) -> Option<ModelAllowlist> {
        self.runtime()
            .models_for(&account.id, account.provider, &self.settings.models)
    }

    /// Whether an account's allowlist allows `model`. The relay uses this
    /// for HTTP bodies and for WebSocket frames.
    pub fn model_allowed(&self, account: &AccountState, model: &str) -> bool {
        model_allowed(self.effective_models(account).as_ref(), model)
    }

    fn update_account_settings(
        &self,
        id: &AccountId,
        change: impl FnOnce(&mut AccountSettings),
    ) -> Result<AccountReport, StateError> {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account = accounts
            .get(id)
            .ok_or_else(|| StateError::NotFound(id.clone()))?;
        let mut runtime = self.runtime();
        self.update_runtime(&mut runtime, |updated| {
            change(updated.accounts.entry(id.clone()).or_default());
        })?;
        Ok(self.account_view(account, &runtime, Utc::now()).report)
    }

    fn update_runtime(
        &self,
        runtime: &mut RuntimeSettings,
        change: impl FnOnce(&mut RuntimeSettings),
    ) -> Result<(), StateError> {
        let mut updated = runtime.clone();
        change(&mut updated);
        updated
            .save(&self.runtime_path)
            .map_err(StateError::Persist)?;
        *runtime = updated;
        self.notify_changed();
        Ok(())
    }

    /// Drop runtime settings of accounts that no longer exist.
    pub(super) fn prune_runtime(&self, keep: impl Fn(&AccountId) -> bool) {
        let mut runtime = self.runtime();
        let before = runtime.accounts.len();
        runtime.accounts.retain(|id, _| keep(id));
        if before != runtime.accounts.len()
            && let Err(err) = runtime.save(&self.runtime_path)
        {
            tracing::warn!("cannot save pruned runtime settings: {err}");
        }
    }
}

fn validate_reserve(reserve: &ReserveConfig) -> Result<(), StateError> {
    let values = [reserve.default]
        .into_iter()
        .chain(reserve.five_hour)
        .chain(reserve.weekly);
    for value in values {
        if !(0.0..=1.0).contains(&value) {
            return Err(StateError::Invalid(format!(
                "reserve {value} is outside 0.0..=1.0"
            )));
        }
    }
    Ok(())
}
