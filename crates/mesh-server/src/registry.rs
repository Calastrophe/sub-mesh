//! In-memory view of the mesh and the scheduling rules on top of it.
//!
//! Callers pass the current time explicitly and handle persistence separately.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::{AccountId, AccountReport, DaemonReport};

mod balancer;
mod control;
mod in_flight;
mod models;
mod scheduling;
mod snapshot;
mod tunnel;
mod warmth;

pub use control::ControlTarget;
pub use in_flight::{InFlight, InFlightGuard};
pub use scheduling::{Candidate, DAEMON_COOLDOWN, DEFAULT_LIMITED_COOLDOWN, DENIED_COOLDOWN};
pub use snapshot::{AccountView, Snapshot};
pub use warmth::{Caller, MAX_KEY_BYTES, Scope, WARMTH_LIMIT, Warmth, WarmthKey, WarmthRow};

use scheduling::active;
use tunnel::TunnelInfo;

/// A daemon row as stored in the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonRow {
    pub node: String,
    /// Tailnet login of the daemon's machine, when known.
    pub owner: Option<String>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug)]
struct Daemon {
    owner: Option<String>,
    last_seen: DateTime<Utc>,
    /// False for a daemon loaded from the database that has not reported
    /// since the server started. Such a daemon counts as stale.
    reported: bool,
    /// Rest after a tunnel failure. A new tunnel clears it.
    cooldown_until: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct Account {
    report: AccountReport,
    node: String,
    cooldown_until: Option<DateTime<Utc>>,
    in_flight: InFlight,
    /// The model ids the provider last listed for the account, sorted and
    /// before the allowlist. Empty until a client fetches the model list.
    listed_models: Vec<String>,
}

/// The registry of daemons and accounts, plus which accounts hold warm
/// prompt caches.
///
/// A daemon can serve requests only while its tunnel is attached
/// ([`Registry::attach_tunnel`]) and its reports are fresh. The registry
/// tracks which tunnel is current; the transport itself lives elsewhere.
#[derive(Debug)]
pub struct Registry {
    stale_after: TimeDelta,
    insecure_skip_whois: bool,
    /// The headroom an account needs to keep its sessions.
    affinity_min_headroom: f64,
    daemons: BTreeMap<String, Daemon>,
    accounts: BTreeMap<AccountId, Account>,
    /// The current tunnel of each connected daemon, by node.
    tunnels: BTreeMap<String, TunnelInfo>,
    /// The generation given to the next tunnel.
    next_generation: u64,
    /// The account that last used each warm cache. See [`warmth`].
    warmth: HashMap<WarmthKey, Warmth>,
    /// See [`Registry::models_revision`].
    models_revision: u64,
}

impl Registry {
    pub fn new(
        stale_after: TimeDelta,
        affinity_min_headroom: f64,
        insecure_skip_whois: bool,
    ) -> Registry {
        Registry {
            stale_after,
            insecure_skip_whois,
            affinity_min_headroom,
            daemons: BTreeMap::new(),
            accounts: BTreeMap::new(),
            tunnels: BTreeMap::new(),
            next_generation: 1,
            warmth: HashMap::new(),
            models_revision: 0,
        }
    }

    /// Fill the registry from the database. Loaded daemons count as stale
    /// until they send a fresh report.
    pub fn load(
        &mut self,
        daemons: impl IntoIterator<Item = DaemonRow>,
        accounts: impl IntoIterator<Item = (String, AccountReport)>,
    ) {
        for row in daemons {
            self.daemons.insert(
                row.node,
                Daemon {
                    owner: row.owner,
                    last_seen: row.last_seen,
                    reported: false,
                    cooldown_until: None,
                },
            );
        }
        for (node, report) in accounts {
            self.accounts.insert(
                report.id.clone(),
                Account {
                    report,
                    node,
                    cooldown_until: None,
                    in_flight: InFlight::default(),
                    listed_models: Vec::new(),
                },
            );
        }
    }

    /// Discard claims to accounts that still belong to a live daemon.
    pub fn filter_report(&self, report: &mut DaemonReport, now: DateTime<Utc>) {
        report.accounts.retain(|claim| {
            let owner = self.accounts.get(&claim.id).filter(|owner| {
                owner.node != report.node
                    && self
                        .daemons
                        .get(&owner.node)
                        .is_some_and(|daemon| self.heard_recently(daemon, now))
            });
            if let Some(owner) = owner {
                tracing::warn!(
                    account = %claim.id, owner = %owner.node, node = %report.node,
                    "ignoring account claimed by another daemon"
                );
            }
            owner.is_none()
        });
    }

    /// The owner login last recorded for `node`, if any.
    pub fn daemon_owner(&self, node: &str) -> Option<String> {
        self.daemons
            .get(node)
            .and_then(|daemon| daemon.owner.clone())
    }

    /// A recently seen daemon or open tunnel holds its node name against
    /// another login.
    pub fn node_taken(&self, node: &str, owner: Option<&str>, now: DateTime<Utc>) -> bool {
        let daemon_taken = self.daemons.get(node).is_some_and(|daemon| {
            self.heard_recently(daemon, now)
                && (self.conflicting_owners(daemon.owner.as_deref(), owner)
                    || (daemon.owner.is_some() && owner.is_none()))
        });
        let tunnel_taken = self
            .tunnels
            .get(node)
            .is_some_and(|tunnel| self.conflicting_owners(tunnel.login.as_deref(), owner));
        daemon_taken || tunnel_taken
    }

    fn conflicting_owners(&self, current: Option<&str>, incoming: Option<&str>) -> bool {
        match (current, incoming) {
            (Some(current), Some(incoming)) => !current.eq_ignore_ascii_case(incoming),
            (None, None) => false,
            _ => !self.insecure_skip_whois,
        }
    }

    /// Apply a filtered daemon report from the machine of `owner`. Accounts
    /// that the node no longer reports are removed.
    pub fn apply_report(
        &mut self,
        report: DaemonReport,
        owner: Option<String>,
        now: DateTime<Utc>,
    ) {
        let node = report.node;
        let cooldown_until = self
            .daemons
            .get(&node)
            .and_then(|daemon| daemon.cooldown_until);
        self.daemons.insert(
            node.clone(),
            Daemon {
                owner,
                last_seen: now,
                reported: true,
                cooldown_until,
            },
        );
        let reported_ids: HashSet<_> = report.accounts.iter().map(|account| &account.id).collect();
        let mut models_changed = false;
        self.accounts.retain(|id, account| {
            let keep = account.node != node || reported_ids.contains(id);
            models_changed |= !keep;
            keep
        });
        for account in report.accounts {
            let existing = self.accounts.get(&account.id);
            models_changed |= existing.is_none_or(|existing| {
                existing.node != node
                    || existing.report.provider != account.provider
                    || existing.report.models != account.models
            });
            let same_node = existing.filter(|existing| existing.node == node);
            let cooldown_until = same_node.and_then(|existing| existing.cooldown_until);
            let listed_models = same_node
                .map(|existing| existing.listed_models.clone())
                .unwrap_or_default();
            let in_flight = existing.map(|existing| existing.in_flight.clone());
            self.accounts.insert(
                account.id.clone(),
                Account {
                    report: account,
                    node: node.clone(),
                    cooldown_until,
                    in_flight: in_flight.unwrap_or_default(),
                    listed_models,
                },
            );
        }
        if models_changed {
            self.touch_models();
        }
    }

    /// Whether `node` has a tunnel and reports that are fresh.
    fn daemon_online(&self, node: &str, daemon: &Daemon, now: DateTime<Utc>) -> bool {
        daemon.reported
            && self.heard_recently(daemon, now)
            && self.tunnel_matches_owner(node, daemon.owner.as_deref())
    }

    fn tunnel_matches_owner(&self, node: &str, owner: Option<&str>) -> bool {
        self.tunnels
            .get(node)
            .is_some_and(|tunnel| !self.conflicting_owners(tunnel.login.as_deref(), owner))
    }

    fn heard_recently(&self, daemon: &Daemon, now: DateTime<Utc>) -> bool {
        now - daemon.last_seen <= self.stale_after
    }

    /// Drop cold caches and expired cooldowns.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.warmth
            .retain(|_, warmth| warmth.is_warm(now) && self.accounts.contains_key(&warmth.account));
        for daemon in self.daemons.values_mut() {
            daemon.cooldown_until = active(daemon.cooldown_until, now);
        }
        for account in self.accounts.values_mut() {
            account.cooldown_until = active(account.cooldown_until, now);
        }
    }
}

#[cfg(test)]
mod tests;
