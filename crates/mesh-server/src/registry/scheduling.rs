use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::protocol::HeadroomHint;
use mesh_core::{AccountId, Admission, DenyReason, Provider, Score};

use super::balancer;
use super::{Account, Caller, Daemon, InFlightGuard, Registry};

/// An account that may serve a request right now.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub id: AccountId,
    pub node: String,
    pub owner: Option<String>,
    /// The generation of the daemon's tunnel when the candidate was selected.
    pub generation: u64,
    /// The daemon's view of the room left before the reserve, from 0.0 to
    /// 1.0. Session affinity gives up on an account when this gets too low.
    pub headroom: f64,
    /// The balancer's score at the time the candidate was selected.
    pub score: Score,
}

/// How long an account rests after its daemon refuses a request.
pub const DENIED_COOLDOWN: TimeDelta = TimeDelta::seconds(30);
/// How long a whole daemon rests after a connection error.
pub const DAEMON_COOLDOWN: TimeDelta = TimeDelta::seconds(15);
/// Cooldown after an upstream `429` that has no `retry-after`.
pub const DEFAULT_LIMITED_COOLDOWN: TimeDelta = TimeDelta::seconds(60);

fn extend_cooldown(slot: &mut Option<DateTime<Utc>>, until: DateTime<Utc>) {
    *slot = Some(slot.map_or(until, |current| current.max(until)));
}

pub(super) fn active(until: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    until.filter(|until| *until > now)
}

impl Registry {
    /// Check that the account is still routed to the selected daemon and
    /// that the daemon is still on the tunnel the candidate was selected on.
    pub fn is_current_candidate(&self, candidate: &Candidate, provider: Provider) -> bool {
        self.accounts.get(&candidate.id).is_some_and(|account| {
            account.node == candidate.node && account.report.provider == provider
        }) && self.daemons.get(&candidate.node).is_some_and(|daemon| {
            !self.conflicting_owners(daemon.owner.as_deref(), candidate.owner.as_deref())
        }) && self
            .tunnels
            .get(&candidate.node)
            .is_some_and(|tunnel| tunnel.generation == candidate.generation)
    }

    #[cfg(test)]
    pub fn usable(&self, id: &AccountId, now: DateTime<Utc>) -> Option<Candidate> {
        self.candidate(self.accounts.get(id)?, now)
    }

    fn candidate(&self, account: &Account, now: DateTime<Utc>) -> Option<Candidate> {
        let daemon = self.daemons.get(&account.node)?;
        let headroom = self.usable_headroom(account, daemon, now)?;
        let generation = self.tunnels.get(&account.node)?.generation;
        Some(Candidate {
            id: account.report.id.clone(),
            node: account.node.clone(),
            owner: daemon.owner.clone(),
            generation,
            headroom,
            score: account.score(account.in_flight.count(), now),
        })
    }

    pub(super) fn usable_headroom(
        &self,
        account: &Account,
        daemon: &Daemon,
        now: DateTime<Utc>,
    ) -> Option<f64> {
        if !self.daemon_online(&account.node, daemon, now)
            || account.active_cooldown(daemon, now).is_some()
        {
            return None;
        }
        account
            .report
            .admission
            .headroom()
            .filter(|headroom| (0.0..=1.0).contains(headroom))
    }

    fn usable_candidates(
        &self,
        provider: Provider,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Vec<Candidate> {
        let usable: Vec<(Candidate, Option<bool>)> = self
            .accounts
            .values()
            .filter(|account| account.report.provider == provider)
            .filter(|account| model.is_none_or(|model| account.allows(model)))
            .filter_map(|account| {
                let lists = model.and_then(|model| account.lists(model));
                self.candidate(account, now)
                    .map(|candidate| (candidate, lists))
            })
            .collect();
        let any_lists = usable.iter().any(|(_, lists)| *lists == Some(true));
        usable
            .into_iter()
            .filter(|(_, lists)| !any_lists || *lists != Some(false))
            .map(|(candidate, _)| candidate)
            .collect()
    }

    /// Accounts of `provider` that may serve a request from `caller`, in the
    /// order to try them: the session's warm account first while it keeps
    /// enough headroom, then the best score first. See [`balancer`] for the
    /// rules.
    ///
    /// A request for `model` goes only to accounts whose allowlist allows
    /// it. When some of those last listed the model, accounts whose known
    /// list leaves it out are skipped, so a model of one plan goes to
    /// accounts on that plan. A model that no usable account listed, such as
    /// an alias or a new release, may go to any of them.
    pub fn candidates(
        &self,
        provider: Provider,
        caller: &Caller<'_>,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Vec<Candidate> {
        let list = self.usable_candidates(provider, model, now);
        let warm = self.warm_accounts(provider, caller, now);
        balancer::order(list, warm, self.affinity_min_headroom)
    }

    /// Every account of `provider` that may serve a request now, ordered by
    /// account id.
    pub fn eligible(&self, provider: Provider, now: DateTime<Utc>) -> Vec<Candidate> {
        self.usable_candidates(provider, None, now)
    }

    /// Start counting a request on `candidate`, if it may still serve one.
    /// The request counts until the guard is dropped.
    pub fn begin(
        &self,
        candidate: &Candidate,
        provider: Provider,
        now: DateTime<Utc>,
    ) -> Option<InFlightGuard> {
        if !self.is_current_candidate(candidate, provider) {
            return None;
        }
        let account = self.accounts.get(&candidate.id)?;
        let daemon = self.daemons.get(&account.node)?;
        self.usable_headroom(account, daemon, now)?;
        Some(account.in_flight.start())
    }

    /// Rest an account until `until`. A longer cooldown is never shortened.
    pub fn cooldown_account(&mut self, id: &AccountId, until: DateTime<Utc>) {
        if let Some(account) = self.accounts.get_mut(id) {
            extend_cooldown(&mut account.cooldown_until, until);
        }
    }

    /// Rest a whole daemon until `until`.
    pub fn cooldown_daemon(&mut self, node: &str, until: DateTime<Utc>) {
        if let Some(daemon) = self.daemons.get_mut(node) {
            extend_cooldown(&mut daemon.cooldown_until, until);
        }
    }

    /// Apply the headroom a daemon sent with a response. `Deny` holds until
    /// the next report. A number only updates an account that still admits.
    pub fn apply_headroom(&mut self, id: &AccountId, hint: HeadroomHint) {
        let Some(account) = self.accounts.get_mut(id) else {
            return;
        };
        match hint {
            HeadroomHint::Deny => {
                account.report.admission = Admission::Deny {
                    reason: DenyReason::Limited {
                        window: "upstream".to_owned(),
                    },
                };
            }
            HeadroomHint::Value(headroom) => {
                if account.report.admission.is_admit() {
                    account.report.admission = Admission::Admit { headroom };
                }
            }
        }
    }
}

impl Account {
    pub(super) fn active_cooldown(
        &self,
        daemon: &Daemon,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        active(self.cooldown_until.max(daemon.cooldown_until), now)
    }

    pub(super) fn score(&self, in_flight: u32, now: DateTime<Utc>) -> Score {
        Score::new(&self.report.windows, &self.report.reserve, in_flight, now)
    }
}
