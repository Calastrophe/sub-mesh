use chrono::{DateTime, Utc};
use mesh_core::api::DaemonInfo;
use mesh_core::{AccountId, AccountReport, Admission, DenyReason};

use super::{Account, Daemon, Registry};

/// The part of a [`Registry`] the management API shows.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub daemons: Vec<DaemonInfo>,
    pub accounts: Vec<AccountView>,
}

#[derive(Clone, Debug)]
pub struct AccountView {
    pub report: AccountReport,
    pub node: String,
    /// Tailnet login of the daemon's machine, when known.
    pub owner: Option<String>,
    /// Whether the account's daemon has a tunnel and reported recently.
    pub online: bool,
    /// Requests being relayed through the account now.
    pub in_flight: u32,
    /// Set while the account or its daemon rests after an error or refusal.
    pub cooldown_until: Option<DateTime<Utc>>,
    /// The daemon's decision, or a refusal while a cooldown holds.
    pub admission: Admission,
    /// The balancer's score. `None` when the account is not a candidate.
    pub score: Option<f64>,
    /// The model ids the provider last listed for the account, sorted.
    pub available_models: Vec<String>,
}

impl Registry {
    /// A copy of the state at `now`, with daemons ordered by node and
    /// accounts by id.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Snapshot {
        Snapshot {
            daemons: self
                .daemons
                .iter()
                .map(|(node, daemon)| self.daemon_view(node, daemon, now))
                .collect(),
            accounts: self
                .accounts
                .values()
                .filter_map(|account| self.account_view(account, now))
                .collect(),
        }
    }

    /// One account as the management API shows it.
    pub fn view(&self, id: &AccountId, now: DateTime<Utc>) -> Option<AccountView> {
        self.accounts
            .get(id)
            .and_then(|account| self.account_view(account, now))
    }

    fn daemon_view(&self, node: &str, daemon: &Daemon, now: DateTime<Utc>) -> DaemonInfo {
        DaemonInfo {
            node: node.to_owned(),
            owner: daemon.owner.clone(),
            connected_since: self.tunnels.get(node).map(|tunnel| tunnel.connected_since),
            last_seen: daemon.last_seen,
            online: self.daemon_online(node, daemon, now),
        }
    }

    fn account_view(&self, account: &Account, now: DateTime<Utc>) -> Option<AccountView> {
        let daemon = self.daemons.get(&account.node)?;
        let cooldown_until = account.active_cooldown(daemon, now);
        let mut admission = account.report.admission.clone();
        if cooldown_until.is_some() && admission.is_admit() {
            admission = Admission::Deny {
                reason: DenyReason::Limited {
                    window: "cooldown".to_owned(),
                },
            };
        }
        let in_flight = account.in_flight.count();
        Some(AccountView {
            node: account.node.clone(),
            owner: daemon.owner.clone(),
            online: self.daemon_online(&account.node, daemon, now),
            in_flight,
            cooldown_until,
            admission,
            score: self
                .usable_headroom(account, daemon, now)
                .map(|_| account.score(in_flight, now).value),
            available_models: account.listed_models.clone(),
            report: account.report.clone(),
        })
    }
}
