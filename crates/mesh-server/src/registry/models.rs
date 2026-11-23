//! What each account may serve, by model: the owner's allowlist, and the
//! model ids the provider last listed for the account.

use mesh_core::{AccountId, ModelAllowlist, Provider, model_allowed};

use super::{Account, Registry};

impl Account {
    /// Whether the account's allowlist allows `model`.
    pub(super) fn allows(&self, model: &str) -> bool {
        model_allowed(self.report.models.as_ref(), model)
    }

    /// Whether the provider's last model list for the account names
    /// `model`, ignoring case. `None` when no list is known yet.
    pub(super) fn lists(&self, model: &str) -> Option<bool> {
        (!self.listed_models.is_empty()).then(|| {
            self.listed_models
                .iter()
                .any(|listed| listed.eq_ignore_ascii_case(model))
        })
    }
}

impl Registry {
    /// Changes whenever an account appears or goes, or its allowlist changes.
    /// A merged model list made at an older revision is out of date.
    pub fn models_revision(&self) -> u64 {
        self.models_revision
    }

    pub(super) fn touch_models(&mut self) {
        self.models_revision += 1;
    }

    /// Remember the model ids the provider listed for account `id`, before
    /// its allowlist. An empty list says nothing and is ignored, so an
    /// account is never left unable to serve any model.
    pub fn record_models(&mut self, id: &AccountId, mut models: Vec<String>) {
        if models.is_empty() {
            return;
        }
        models.sort_unstable();
        models.dedup();
        if let Some(account) = self.accounts.get_mut(id) {
            account.listed_models = models;
        }
    }

    /// The effective model allowlist of account `id`.
    pub fn model_allowlist(&self, id: &AccountId) -> Option<ModelAllowlist> {
        self.accounts
            .get(id)
            .and_then(|account| account.report.models.clone())
    }

    /// Whether `provider` has accounts and none of them, in whatever state,
    /// allows `model`. An account that merely lacks room still counts as
    /// able to serve it.
    pub fn model_unserved(&self, provider: Provider, model: &str) -> bool {
        let mut accounts = self
            .accounts
            .values()
            .filter(|account| account.report.provider == provider)
            .peekable();
        accounts.peek().is_some() && !accounts.any(|account| account.allows(model))
    }
}
