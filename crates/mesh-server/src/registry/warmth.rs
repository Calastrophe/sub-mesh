//! Which accounts hold a warm prompt cache, and for whom.

use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::{AccountId, Provider};

use super::Registry;
use super::balancer::Warm;

/// The most entries kept. A new entry past the limit is dropped until
/// [`Registry::prune`] frees room.
pub const WARMTH_LIMIT: usize = 16_384;

/// Maximum size of a session or user key in bytes.
pub const MAX_KEY_BYTES: usize = 512;

/// Whose prompt cache an entry describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// One conversation. Its cache holds the conversation's full prefix.
    Session,
    /// One user across all their conversations. Its cache holds the prefix
    /// that the conversations share: the system prompt and the tools.
    User,
}

impl Scope {
    const ALL: [Scope; 2] = [Scope::Session, Scope::User];

    /// The name used in storage.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Session => "session",
            Scope::User => "user",
        }
    }

    /// The scope that [`Scope::as_str`] gives `text` for.
    pub fn parse(text: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.as_str() == text)
    }
}

/// What a warm cache is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WarmthKey {
    pub provider: Provider,
    pub scope: Scope,
    /// The session key, or the user's login or node name.
    pub name: String,
}

/// The account that used a scope last, and for how long the use keeps the
/// cache warm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warmth {
    pub account: AccountId,
    pub last_used: DateTime<Utc>,
    pub ttl: TimeDelta,
}

/// An entry as stored.
pub type WarmthRow = (WarmthKey, Warmth);

impl Warmth {
    /// Whether the cache is still warm at `now`, at storage precision.
    pub fn is_warm(&self, now: DateTime<Utc>) -> bool {
        self.expires() >= now.timestamp_micros()
    }

    fn expires(&self) -> i64 {
        self.last_used
            .checked_add_signed(self.ttl)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
            .timestamp_micros()
    }
}

impl WarmthKey {
    fn new(provider: Provider, scope: Scope, name: &str) -> Option<Self> {
        if name.is_empty() || name.len() > MAX_KEY_BYTES {
            return None;
        }
        let name = match scope {
            Scope::Session => name.to_owned(),
            Scope::User => name.to_ascii_lowercase(),
        };
        Some(Self {
            provider,
            scope,
            name,
        })
    }
}

/// Whose caches a request can hit. The default caller has no session and
/// an empty user, so it hits none.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller<'a> {
    /// The conversation key, when the request has one.
    pub session: Option<&'a str>,
    /// The caller's login, or its node name when it has no login.
    pub user: &'a str,
}

impl Registry {
    /// Restore entries from the database, up to the limit.
    pub fn load_warmth(&mut self, rows: impl IntoIterator<Item = WarmthRow>) {
        for (key, warmth) in rows {
            if self
                .accounts
                .get(&warmth.account)
                .is_some_and(|account| account.report.provider == key.provider)
                && let Some(key) = WarmthKey::new(key.provider, key.scope, &key.name)
            {
                self.remember(key, warmth);
            }
        }
    }

    pub(super) fn warm_accounts(
        &self,
        provider: Provider,
        caller: &Caller<'_>,
        now: DateTime<Utc>,
    ) -> Warm<'_> {
        Warm {
            session: caller
                .session
                .and_then(|name| self.warm_account(provider, Scope::Session, name, now)),
            user: self.warm_account(provider, Scope::User, caller.user, now),
        }
    }

    fn warm_account(
        &self,
        provider: Provider,
        scope: Scope,
        name: &str,
        now: DateTime<Utc>,
    ) -> Option<&AccountId> {
        let key = WarmthKey::new(provider, scope, name)?;
        self.warmth
            .get(&key)
            .filter(|warmth| warmth.is_warm(now))
            .map(|warmth| &warmth.account)
    }

    /// Remember that `account` served `caller`, which keeps the caches of
    /// the session and the user warm for `ttl`. Returns the entries that
    /// changed, for the database.
    pub fn record_warmth(
        &mut self,
        provider: Provider,
        caller: &Caller<'_>,
        account: &AccountId,
        ttl: TimeDelta,
        now: DateTime<Utc>,
    ) -> Vec<WarmthRow> {
        if ttl <= TimeDelta::zero() {
            return Vec::new();
        }
        let session = caller.session.map(|name| (Scope::Session, name));
        session
            .into_iter()
            .chain([(Scope::User, caller.user)])
            .filter_map(|(scope, name)| {
                let key = WarmthKey::new(provider, scope, name)?;
                let warmth = Warmth {
                    account: account.clone(),
                    last_used: now,
                    ttl,
                };
                self.remember(key, warmth)
            })
            .collect()
    }

    /// Store `warmth`. The same account with a cache that is still warm keeps
    /// the longest lifetime seen, so a request that does not state its lifetime
    /// (or a body too large to read it from) does not shorten the cache of an
    /// earlier one. A different account, or a cold entry, takes `warmth` as it
    /// is. Nothing is stored when the held entry already covers `warmth`.
    ///
    /// The user scope merges the same way. Its cache is the shared prefix of
    /// the user's sessions on one account, and a provider refreshes a cache
    /// with the lifetime it was written with when any request reads it.
    fn remember(&mut self, key: WarmthKey, mut warmth: Warmth) -> Option<WarmthRow> {
        match self.warmth.get(&key) {
            Some(held) if held.account == warmth.account => {
                if held.is_warm(warmth.last_used) {
                    warmth.ttl = warmth.ttl.max(held.ttl);
                }
                if held.expires() >= warmth.expires() {
                    return None;
                }
            }
            None if self.warmth.len() >= WARMTH_LIMIT => return None,
            _ => {}
        }
        self.warmth.insert(key.clone(), warmth.clone());
        Some((key, warmth))
    }
}
