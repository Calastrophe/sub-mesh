use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::api::{
    AccessList, AccessRequest, EntrySource, FixedSettings, ListEntry, ListGroup, ListKind,
    MAX_WEB_BODY_BYTES, ProxyInfo, SettingName, SettingValue, SettingsView, Tunable,
};

use crate::config::{Config, PeerPolicy};
use crate::db::{Db, StoredEntry, StoredSettings, StoredValue};
use crate::peer::{Policies, merge_logins, merge_tags};

#[derive(Clone, Debug)]
pub struct Effective {
    pub config: Config,
    pub policies: Policies,
}

impl Effective {
    pub fn stale_after(&self) -> TimeDelta {
        TimeDelta::from_std(self.config.daemon_stale()).unwrap_or(TimeDelta::MAX)
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub stored: StoredSettings,
    pub effective: Arc<Effective>,
}

pub struct Settings {
    snapshot: RwLock<Arc<Snapshot>>,
    writer: Mutex<()>,
}

pub const DENIED_RETENTION: TimeDelta = TimeDelta::days(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid(String),
    Missing(String),
    Fixed(String),
}

fn value_of(config: &Config, name: SettingName) -> SettingValue {
    match name {
        SettingName::CacheTtlSecs => SettingValue::Whole(config.cache_ttl_secs.into()),
        SettingName::AffinityMinHeadroom => SettingValue::Fraction(config.affinity_min_headroom),
        SettingName::MaxAttempts => SettingValue::Whole(config.max_attempts as u64),
        SettingName::DaemonStaleSecs => SettingValue::Whole(config.daemon_stale_secs),
        SettingName::MaxBodyBytes => SettingValue::Whole(config.max_body_bytes as u64),
        SettingName::ConsumersAllowLoopback => SettingValue::Flag(config.consumers.allow_loopback),
        SettingName::DaemonsAllowLoopback => SettingValue::Flag(config.daemons.allow_loopback),
    }
}

pub fn check_value(
    config: &Config,
    name: SettingName,
    value: SettingValue,
) -> Result<SettingValue, String> {
    if name == SettingName::MaxBodyBytes {
        let ceiling = (config.max_body_bytes as u64).max(MAX_WEB_BODY_BYTES);
        let number = value.as_f64();
        let whole = !matches!(value, SettingValue::Flag(_)) && number.fract() == 0.0;
        if whole && number > MAX_WEB_BODY_BYTES as f64 && number <= ceiling as f64 {
            return Ok(SettingValue::Whole(number as u64));
        }
        if number > ceiling as f64 {
            return Err(format!(
                "{name} must be between {} and {ceiling}",
                name.bounds().0
            ));
        }
    }
    name.check(value)
}

fn override_value(
    base: &Config,
    config: &mut Config,
    name: SettingName,
    value: SettingValue,
) -> Result<(), String> {
    let value = check_value(base, name, value)?;
    let whole = || match value {
        SettingValue::Whole(whole) => Ok(whole),
        _ => Err(format!("{name} is a whole number")),
    };
    match name {
        SettingName::CacheTtlSecs => {
            config.cache_ttl_secs = u32::try_from(whole()?).map_err(|e| e.to_string())?;
        }
        SettingName::AffinityMinHeadroom => config.affinity_min_headroom = value.as_f64(),
        SettingName::MaxAttempts => {
            config.max_attempts = usize::try_from(whole()?).map_err(|e| e.to_string())?;
        }
        SettingName::DaemonStaleSecs => config.daemon_stale_secs = whole()?,
        SettingName::MaxBodyBytes => {
            config.max_body_bytes = usize::try_from(whole()?).map_err(|e| e.to_string())?;
        }
        SettingName::ConsumersAllowLoopback => config.consumers.allow_loopback = value.as_flag(),
        SettingName::DaemonsAllowLoopback => config.daemons.allow_loopback = value.as_flag(),
    }
    Ok(())
}

fn policy_mut(policies: &mut Policies, group: ListGroup) -> &mut PeerPolicy {
    match group {
        ListGroup::Consumers => &mut policies.consumers,
        ListGroup::Daemons => &mut policies.daemons,
    }
}

fn policy_of(config: &Config, group: ListGroup) -> &PeerPolicy {
    match group {
        ListGroup::Consumers => &config.consumers,
        ListGroup::Daemons => &config.daemons,
    }
}

pub fn same_value(kind: ListKind, known: &str, value: &str) -> bool {
    match kind {
        ListKind::Logins => known.eq_ignore_ascii_case(value),
        ListKind::Tags => known == value,
    }
}

fn baseline(config: &Config, group: ListGroup, kind: ListKind) -> Vec<(String, EntrySource)> {
    let policy = policy_of(config, group);
    let mut entries: Vec<(String, EntrySource)> = Vec::new();
    let mut push = |value: &String, source| {
        if !entries
            .iter()
            .any(|(known, _)| same_value(kind, known, value))
        {
            entries.push((value.clone(), source));
        }
    };
    match kind {
        ListKind::Logins => {
            for login in &policy.logins {
                push(login, EntrySource::Config);
            }
            if group == ListGroup::Consumers {
                for login in &config.admins.logins {
                    push(login, EntrySource::Admins);
                }
            }
        }
        ListKind::Tags => {
            for tag in &policy.tags {
                push(tag, EntrySource::Config);
            }
        }
    }
    entries
}

fn in_baseline(config: &Config, group: ListGroup, kind: ListKind, value: &str) -> bool {
    baseline(config, group, kind)
        .iter()
        .any(|(known, _)| same_value(kind, known, value))
}

fn same_entry(entry: &StoredEntry, group: ListGroup, kind: ListKind, value: &str) -> bool {
    entry.group == group && entry.kind == kind && same_value(kind, &entry.value, value)
}

pub fn compute(config: &Config, stored: &StoredSettings) -> Result<Effective, String> {
    let mut effective = config.clone();
    for value in &stored.values {
        override_value(config, &mut effective, value.name, value.value)?;
    }
    effective
        .validate_settings()
        .map_err(|error| format!("{error:#}"))?;
    let mut policies = Policies::from_config(&effective);
    for entry in &stored.entries {
        let policy = policy_mut(&mut policies, entry.group);
        let more = std::slice::from_ref(&entry.value);
        match entry.kind {
            ListKind::Logins => merge_logins(&mut policy.logins, more),
            ListKind::Tags => merge_tags(&mut policy.tags, more),
        }
    }
    Ok(Effective {
        config: effective,
        policies,
    })
}

impl Settings {
    pub fn load(config: &Config, db: &Db, now: DateTime<Utc>) -> anyhow::Result<Settings> {
        let loaded = db.load_settings()?;
        let mut stored = StoredSettings::default();
        for entry in loaded.entries {
            match entry.kind.check(&entry.value) {
                Ok(value)
                    if value == entry.value
                        && !in_baseline(config, entry.group, entry.kind, &entry.value) =>
                {
                    stored.entries.push(entry);
                }
                Ok(_) if in_baseline(config, entry.group, entry.kind, &entry.value) => {
                    tracing::info!(
                        list = %format!("{}.{}", entry.group.as_str(), entry.kind.as_str()),
                        value = %entry.value,
                        "removing a web-managed entry that the server config now lists"
                    );
                    db.delete_entry(&entry)?;
                }
                Ok(_) => {
                    tracing::warn!(value = %entry.value, "removing an unnormalized stored access entry");
                    db.delete_entry(&entry)?;
                }
                Err(reason) => {
                    tracing::warn!(value = %entry.value, %reason, "removing an invalid stored access entry");
                    db.delete_entry(&entry)?;
                }
            }
        }
        for value in loaded.values {
            stored.values.push(value.clone());
            if let Err(reason) = compute(config, &stored) {
                tracing::warn!(name = %value.name, %reason, "removing an invalid stored setting override");
                stored.values.pop();
                db.remove_override(value.name)?;
            }
        }
        let effective = compute(config, &stored).map_err(anyhow::Error::msg)?;
        let consumers = db.forget_requests_of(&effective.policies.consumers.logins)?;
        if consumers > 0 {
            tracing::info!(
                count = consumers,
                "removed access requests of logins that are consumers"
            );
        }
        db.expire_denied(now - DENIED_RETENTION)?;
        Ok(Settings {
            snapshot: RwLock::new(Arc::new(Snapshot {
                stored,
                effective: Arc::new(effective),
            })),
            writer: Mutex::new(()),
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot
            .read()
            .expect("settings lock poisoned")
            .clone()
    }

    pub fn effective(&self) -> Arc<Effective> {
        self.snapshot().effective.clone()
    }

    pub fn change(
        &self,
        config: &Config,
        db: &Db,
        change: Change,
        by: Option<&str>,
        now: DateTime<Utc>,
        install: impl FnOnce(&Effective),
    ) -> anyhow::Result<Result<(bool, Arc<Snapshot>), Refusal>> {
        let _writer = self.writer.lock().expect("settings writer poisoned");
        let current = self.snapshot();
        match apply(config, db, &current.stored, change, by, now)? {
            Err(refusal) => Ok(Err(refusal)),
            Ok(None) => Ok(Ok((false, current))),
            Ok(Some((stored, effective))) => {
                let snapshot = Arc::new(Snapshot {
                    stored,
                    effective: Arc::new(effective),
                });
                *self.snapshot.write().expect("settings lock poisoned") = snapshot.clone();
                install(&snapshot.effective);
                Ok(Ok((true, snapshot)))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Change {
    Add(ListGroup, ListKind, String),
    Remove(ListGroup, ListKind, String),
    Set(SettingName, SettingValue),
    Reset(SettingName),
    Approve(String),
    Deny(String),
    Forget(String),
}

type Applied = Option<(StoredSettings, Effective)>;

fn computed(config: &Config, stored: StoredSettings) -> Result<Applied, Refusal> {
    match compute(config, &stored) {
        Ok(effective) => Ok(Some((stored, effective))),
        Err(reason) => Err(Refusal::Invalid(reason)),
    }
}

fn no_request(login: &str) -> Refusal {
    Refusal::Missing(format!("no access request from {login}"))
}

pub fn apply(
    config: &Config,
    db: &Db,
    current: &StoredSettings,
    change: Change,
    by: Option<&str>,
    now: DateTime<Utc>,
) -> anyhow::Result<Result<Applied, Refusal>> {
    let by = by.map(str::to_owned);
    let mut stored = current.clone();
    match change {
        Change::Add(group, kind, value) => {
            let value = match kind.check(&value) {
                Ok(value) => value,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            if in_baseline(config, group, kind, &value)
                || stored
                    .entries
                    .iter()
                    .any(|entry| same_entry(entry, group, kind, &value))
            {
                if (group, kind) == (ListGroup::Consumers, ListKind::Logins) {
                    db.forget_request(&value)?;
                }
                return Ok(Ok(None));
            }
            let entry = StoredEntry {
                group,
                kind,
                value,
                added_by: by,
                added_at: now,
            };
            stored.entries.push(entry.clone());
            let applied = computed(config, stored);
            if applied.is_ok() {
                db.add_entry(&entry)?;
            }
            Ok(applied)
        }
        Change::Remove(group, kind, value) => {
            if in_baseline(config, group, kind, &value) {
                return Ok(Err(Refusal::Fixed(format!(
                    "{value} comes from the server config"
                ))));
            }
            let Some(removed) = stored
                .entries
                .iter()
                .find(|entry| same_entry(entry, group, kind, &value))
                .cloned()
            else {
                return Ok(Err(Refusal::Missing(format!("{value} is not listed"))));
            };
            stored
                .entries
                .retain(|entry| !same_entry(entry, group, kind, &value));
            let applied = computed(config, stored);
            if applied.is_ok() {
                db.delete_entry(&removed)?;
            }
            Ok(applied)
        }
        Change::Set(name, value) => {
            let value = match check_value(config, name, value) {
                Ok(value) => value,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            let stored_value = StoredValue {
                name,
                value,
                set_by: by,
                set_at: now,
            };
            stored.values.retain(|known| known.name != name);
            stored.values.push(stored_value.clone());
            let applied = computed(config, stored);
            if applied.is_ok() {
                db.set_override(&stored_value)?;
            }
            Ok(applied)
        }
        Change::Reset(name) => {
            stored.values.retain(|known| known.name != name);
            let applied = computed(config, stored);
            if applied.is_ok() {
                db.remove_override(name)?;
            }
            Ok(applied)
        }
        Change::Approve(login) => {
            let login = match ListKind::Logins.check(&login) {
                Ok(login) => login,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            let known = in_baseline(config, ListGroup::Consumers, ListKind::Logins, &login)
                || stored
                    .entries
                    .iter()
                    .any(|entry| same_entry(entry, ListGroup::Consumers, ListKind::Logins, &login));
            if known {
                return Ok(if db.forget_request(&login)? {
                    Ok(None)
                } else {
                    Err(no_request(&login))
                });
            }
            let entry = StoredEntry {
                group: ListGroup::Consumers,
                kind: ListKind::Logins,
                value: login.clone(),
                added_by: by,
                added_at: now,
            };
            stored.entries.push(entry.clone());
            let applied = match computed(config, stored) {
                Ok(applied) => applied,
                Err(refusal) => return Ok(Err(refusal)),
            };
            if !db.approve_request(&login, Some(&entry))? {
                return Ok(Err(no_request(&login)));
            }
            Ok(Ok(applied))
        }
        Change::Deny(login) => Ok(if db.deny_request(&login, by.as_deref(), now)? {
            Ok(None)
        } else {
            Err(no_request(&login))
        }),
        Change::Forget(login) => Ok(if db.forget_request(&login)? {
            Ok(None)
        } else {
            Err(no_request(&login))
        }),
    }
}

fn entries(
    config: &Config,
    stored: &StoredSettings,
    group: ListGroup,
    kind: ListKind,
) -> Vec<ListEntry> {
    let mut list: Vec<ListEntry> = baseline(config, group, kind)
        .into_iter()
        .map(|(value, source)| ListEntry {
            value,
            source,
            added_by: None,
            added_at: None,
        })
        .collect();
    for entry in &stored.entries {
        if entry.group == group
            && entry.kind == kind
            && !list
                .iter()
                .any(|known| same_value(kind, &known.value, &entry.value))
        {
            list.push(ListEntry {
                value: entry.value.clone(),
                source: EntrySource::Web,
                added_by: entry.added_by.clone(),
                added_at: Some(entry.added_at),
            });
        }
    }
    list
}

fn access_list(config: &Config, stored: &StoredSettings, group: ListGroup) -> AccessList {
    AccessList {
        logins: entries(config, stored, group, ListKind::Logins),
        tags: entries(config, stored, group, ListKind::Tags),
        own_login: policy_of(config, group).logins.is_empty(),
    }
}

fn fixed(config: &Config) -> FixedSettings {
    let proxy = &config.trusted_proxy;
    FixedSettings {
        listen: config.listen.to_string(),
        db_path: config.db_path.display().to_string(),
        web_dir: config.web_dir.as_ref().map(|dir| dir.display().to_string()),
        tailscale_socket: config.tailscale_socket.display().to_string(),
        admins: config.admins.logins.clone(),
        trusted_proxy: (!proxy.addrs.is_empty()).then(|| ProxyInfo {
            addrs: proxy.addrs.iter().map(ToString::to_string).collect(),
            header: proxy.header.clone(),
            only: proxy.only,
        }),
        insecure_skip_whois: config.insecure_skip_whois,
    }
}

pub fn view(config: &Config, current: &Snapshot, requests: Vec<AccessRequest>) -> SettingsView {
    let values = SettingName::ALL
        .into_iter()
        .map(|name| {
            let stored = current
                .stored
                .values
                .iter()
                .find(|value| value.name == name);
            Tunable {
                name,
                value: value_of(&current.effective.config, name),
                config: value_of(config, name),
                overridden: stored.is_some(),
                set_by: stored.and_then(|value| value.set_by.clone()),
                set_at: stored.map(|value| value.set_at),
            }
        })
        .collect();
    SettingsView {
        consumers: access_list(config, &current.stored, ListGroup::Consumers),
        daemons: access_list(config, &current.stored, ListGroup::Daemons),
        values,
        requests,
        fixed: fixed(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Admins, TrustedProxy};

    fn t0() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn proxy_only() -> Config {
        Config {
            consumers: PeerPolicy {
                logins: vec!["a@example.com".to_owned()],
                ..PeerPolicy::default()
            },
            daemons: PeerPolicy {
                logins: vec!["a@example.com".to_owned()],
                ..PeerPolicy::default()
            },
            admins: Admins {
                logins: vec!["root@example.com".to_owned()],
            },
            trusted_proxy: TrustedProxy {
                addrs: vec!["10.0.0.1".parse().unwrap()],
                only: true,
                ..TrustedProxy::default()
            },
            web_dir: None,
            ..Config::default()
        }
    }

    fn change(
        config: &Config,
        db: &Db,
        settings: &Settings,
        change: Change,
    ) -> Result<bool, Refusal> {
        settings
            .change(config, db, change, Some("root@example.com"), t0(), |_| {})
            .unwrap()
            .map(|(changed, _)| changed)
    }

    #[test]
    fn effective_settings_keep_the_proxy_only_rules() {
        let config = proxy_only();
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&config, &db, t0()).unwrap();
        let refused = change(
            &config,
            &db,
            &settings,
            Change::Set(
                SettingName::ConsumersAllowLoopback,
                SettingValue::Flag(true),
            ),
        );
        assert!(
            matches!(refused, Err(Refusal::Invalid(reason)) if reason.contains("allow_loopback"))
        );
        assert!(!settings.effective().config.consumers.allow_loopback);
        assert!(db.load_settings().unwrap().values.is_empty());
        assert_eq!(
            change(
                &config,
                &db,
                &settings,
                Change::Set(SettingName::DaemonsAllowLoopback, SettingValue::Flag(false)),
            ),
            Ok(true)
        );
    }

    #[test]
    fn admins_are_consumers_and_the_baseline_stays() {
        let config = proxy_only();
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&config, &db, t0()).unwrap();
        assert!(
            settings
                .effective()
                .policies
                .consumers
                .logins
                .contains(&"root@example.com".to_owned())
        );
        for login in ["a@example.com", "ROOT@example.com"] {
            assert!(matches!(
                change(
                    &config,
                    &db,
                    &settings,
                    Change::Remove(ListGroup::Consumers, ListKind::Logins, login.to_owned()),
                ),
                Err(Refusal::Fixed(_))
            ));
        }
        assert_eq!(
            change(
                &config,
                &db,
                &settings,
                Change::Add(
                    ListGroup::Consumers,
                    ListKind::Logins,
                    "A@example.com".to_owned()
                ),
            ),
            Ok(false)
        );
        assert!(db.load_settings().unwrap().entries.is_empty());
    }

    #[test]
    fn bad_stored_overrides_are_removed_at_load() {
        let db = Db::open_in_memory().unwrap();
        db.set_override(&StoredValue {
            name: SettingName::ConsumersAllowLoopback,
            value: SettingValue::Flag(true),
            set_by: None,
            set_at: t0(),
        })
        .unwrap();
        db.set_override(&StoredValue {
            name: SettingName::MaxAttempts,
            value: SettingValue::Whole(5),
            set_by: None,
            set_at: t0(),
        })
        .unwrap();
        let config = proxy_only();
        let settings = Settings::load(&config, &db, t0()).unwrap();
        let effective = settings.effective();
        assert!(!effective.config.consumers.allow_loopback);
        assert_eq!(effective.config.max_attempts, 5);
        let names: Vec<_> = db
            .load_settings()
            .unwrap()
            .values
            .into_iter()
            .map(|value| value.name)
            .collect();
        assert_eq!(names, [SettingName::MaxAttempts]);
        let open = Settings::load(
            &Config {
                web_dir: None,
                ..Config::default()
            },
            &db,
            t0(),
        )
        .unwrap();
        assert!(!open.effective().config.consumers.allow_loopback);
        assert_eq!(open.effective().config.max_attempts, 5);
    }

    #[test]
    fn web_entries_that_the_config_now_lists_are_removed_at_load() {
        let db = Db::open_in_memory().unwrap();
        for (group, kind, value) in [
            (ListGroup::Consumers, ListKind::Logins, "A@example.com"),
            (ListGroup::Consumers, ListKind::Logins, "Root@example.com"),
            (ListGroup::Consumers, ListKind::Logins, "bob@example.com"),
            (ListGroup::Daemons, ListKind::Tags, "tag:mesh"),
        ] {
            db.add_entry(&StoredEntry {
                group,
                kind,
                value: value.to_owned(),
                added_by: None,
                added_at: t0(),
            })
            .unwrap();
        }
        let config = Config {
            daemons: PeerPolicy {
                logins: vec!["a@example.com".to_owned()],
                tags: vec!["tag:Mesh".to_owned()],
                ..PeerPolicy::default()
            },
            ..proxy_only()
        };
        let settings = Settings::load(&config, &db, t0()).unwrap();
        let kept: Vec<_> = db
            .load_settings()
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.value)
            .collect();
        assert_eq!(kept, ["bob@example.com", "tag:mesh"]);
        assert_eq!(settings.snapshot().stored.entries.len(), 2);
        let without = Settings::load(
            &Config {
                consumers: PeerPolicy {
                    logins: vec!["c@example.com".to_owned()],
                    ..PeerPolicy::default()
                },
                admins: Admins::default(),
                ..proxy_only()
            },
            &db,
            t0(),
        )
        .unwrap();
        let consumers = &without.effective().policies.consumers.logins;
        assert!(
            !consumers
                .iter()
                .any(|login| login.eq_ignore_ascii_case("a@example.com"))
        );
        assert!(
            !consumers
                .iter()
                .any(|login| login.eq_ignore_ascii_case("root@example.com"))
        );
    }

    #[test]
    fn readers_see_the_new_settings_while_a_change_is_still_open() {
        let config = proxy_only();
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&config, &db, t0()).unwrap();
        let mut seen = None;
        let changed = settings
            .change(
                &config,
                &db,
                Change::Set(SettingName::MaxAttempts, SettingValue::Whole(6)),
                None,
                t0(),
                |_| seen = Some(settings.effective().config.max_attempts),
            )
            .unwrap();
        assert!(matches!(changed, Ok((true, _))));
        assert_eq!(seen, Some(6));
    }

    #[test]
    fn tags_match_exactly() {
        let config = Config {
            daemons: PeerPolicy {
                logins: vec!["a@example.com".to_owned()],
                tags: vec!["tag:mesh".to_owned()],
                ..PeerPolicy::default()
            },
            ..proxy_only()
        };
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&config, &db, t0()).unwrap();
        let tag = |value: &str| Change::Add(ListGroup::Daemons, ListKind::Tags, value.to_owned());
        assert_eq!(change(&config, &db, &settings, tag("tag:mesh")), Ok(false));
        assert_eq!(change(&config, &db, &settings, tag("tag:Mesh")), Ok(true));
        assert_eq!(change(&config, &db, &settings, tag("tag:Mesh")), Ok(false));
        assert_eq!(
            settings.effective().policies.daemons.tags,
            ["tag:mesh", "tag:Mesh"]
        );
        assert!(matches!(
            change(
                &config,
                &db,
                &settings,
                Change::Remove(ListGroup::Daemons, ListKind::Tags, "TAG:MESH".to_owned())
            ),
            Err(Refusal::Missing(_))
        ));
        assert_eq!(
            change(
                &config,
                &db,
                &settings,
                Change::Remove(ListGroup::Daemons, ListKind::Tags, "tag:Mesh".to_owned())
            ),
            Ok(true)
        );
        assert_eq!(settings.effective().policies.daemons.tags, ["tag:mesh"]);
    }

    #[test]
    fn the_body_limit_ceiling_follows_the_config() {
        let low = proxy_only();
        let high = Config {
            max_body_bytes: 300 * 1024 * 1024,
            ..proxy_only()
        };
        let at = |config: &Config, bytes: u64| {
            check_value(
                config,
                SettingName::MaxBodyBytes,
                SettingValue::Whole(bytes),
            )
        };
        assert!(at(&low, MAX_WEB_BODY_BYTES).is_ok());
        assert!(at(&low, MAX_WEB_BODY_BYTES + 1).is_err());
        assert_eq!(
            at(&high, 300 * 1024 * 1024),
            Ok(SettingValue::Whole(300 * 1024 * 1024))
        );
        assert!(at(&high, 300 * 1024 * 1024 + 1).is_err());
        assert!(at(&high, 1024).is_err());
    }
}
