use mesh_core::{AccountId, AccountReport, Provider};

use super::Registry;

/// Where to send a control call for one account, and whose account it is.
/// The call goes over the tunnel with this `generation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlTarget {
    pub id: AccountId,
    pub node: String,
    pub provider: Provider,
    /// The generation of the daemon's tunnel, or `None` while it has none.
    pub generation: Option<u64>,
    /// Tailnet login of the daemon's machine, when known.
    pub owner: Option<String>,
}

impl Registry {
    /// The daemon behind account `id`, or `None` for an unknown account.
    pub fn control_target(&self, id: &AccountId) -> Option<ControlTarget> {
        let account = self.accounts.get(id)?;
        let daemon = self.daemons.get(&account.node)?;
        Some(ControlTarget {
            id: id.clone(),
            node: account.node.clone(),
            provider: account.report.provider,
            generation: self
                .tunnels
                .get(&account.node)
                .filter(|_| self.tunnel_matches_owner(&account.node, daemon.owner.as_deref()))
                .map(|tunnel| tunnel.generation),
            owner: daemon.owner.clone(),
        })
    }

    /// Check that an outstanding command still refers to this route and owner.
    fn matches_control_target(&self, target: &ControlTarget) -> bool {
        let Some(current) = self.control_target(&target.id) else {
            return false;
        };
        let same_owner = match (current.owner.as_deref(), target.owner.as_deref()) {
            (Some(current), Some(owner)) => current.eq_ignore_ascii_case(owner),
            (None, None) => true,
            _ => false,
        };
        same_owner
            && current.node == target.node
            && current.generation == target.generation
            && current.provider == target.provider
    }

    /// Apply a control reply only if the account is still at the target.
    pub fn replace_account(&mut self, target: &ControlTarget, report: AccountReport) -> bool {
        if report.id != target.id
            || report.provider != target.provider
            || !self.matches_control_target(target)
        {
            return false;
        }
        let Some(account) = self.accounts.get_mut(&target.id) else {
            return false;
        };
        let models_changed = account.report.models != report.models;
        account.report = report;
        if models_changed {
            self.touch_models();
        }
        true
    }

    /// Forget an account that its daemon deleted.
    pub fn remove_account(&mut self, target: &ControlTarget) -> bool {
        if !self.accounts.contains_key(&target.id) {
            return true;
        }
        let removed =
            self.matches_control_target(target) && self.accounts.remove(&target.id).is_some();
        if removed {
            self.touch_models();
        }
        removed
    }
}
