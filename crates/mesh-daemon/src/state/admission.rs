use std::sync::Arc;

use chrono::{DateTime, Utc};
use mesh_core::{Admission, DenyReason, ReserveConfig, admission};

use super::account::AccountInner;
use super::{AccountState, AppState};

/// True when the change is worth an immediate report. Headroom drifting
/// between two admits is not.
pub(super) fn materially_changed(before: &Admission, after: &Admission) -> bool {
    match (before, after) {
        (Admission::Admit { .. }, Admission::Admit { .. }) => false,
        (Admission::Deny { reason: a }, Admission::Deny { reason: b }) => a != b,
        _ => true,
    }
}

impl AccountInner {
    pub(super) fn admission(
        &self,
        paused: bool,
        reserve: &ReserveConfig,
        now: DateTime<Utc>,
    ) -> Admission {
        if paused {
            Admission::Deny {
                reason: DenyReason::Paused,
            }
        } else if self.auth_error.is_some() {
            Admission::Deny {
                reason: DenyReason::AuthError,
            }
        } else {
            admission(&self.windows, reserve, now)
        }
    }
}

impl AppState {
    /// The current admission decision for `account`.
    pub fn admission(&self, account: &AccountState, now: DateTime<Utc>) -> Admission {
        let (paused, reserve) = self.paused_and_reserve(&account.id);
        let inner = account.inner();
        inner.admission(paused, &reserve, now)
    }

    /// Admission for a relay whose account may have been removed or replaced.
    pub fn current_admission(
        &self,
        account: &Arc<AccountState>,
        now: DateTime<Utc>,
    ) -> Option<Admission> {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let current = accounts.get(&account.id)?;
        Arc::ptr_eq(current, account).then(|| self.admission(account, now))
    }

    /// Change an account and return the closure's result with the new
    /// admission. Asks for a prompt report if the decision changed.
    pub fn mutate<R>(
        &self,
        account: &AccountState,
        change: impl FnOnce(&mut AccountInner) -> R,
    ) -> (R, Admission) {
        let (paused, reserve) = self.paused_and_reserve(&account.id);
        let now = Utc::now();
        let mut inner = account.inner();
        let before = inner.admission(paused, &reserve, now);
        let result = change(&mut inner);
        let after = inner.admission(paused, &reserve, now);
        drop(inner);
        if materially_changed(&before, &after) {
            self.notify_changed();
        }
        (result, after)
    }
}
