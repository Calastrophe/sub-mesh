use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::api::{
    AccessList, AccessRequest, EntrySource, FixedSettings, ListEntry, ListGroup, ListKind,
    ProxyInfo, SettingName, SettingValue, SettingsView, Tunable,
};

use crate::config::{Config, PeerPolicy};
use crate::db::{Db, StoredEntry, StoredSettings, StoredValue};
use crate::peer::{Policies, merge_logins};

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
pub struct Current {
    pub stored: StoredSettings,
    pub effective: Arc<Effective>,
}

pub struct Settings {
    current: Mutex<Current>,
}

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

fn override_value(
    config: &mut Config,
    name: SettingName,
    value: SettingValue,
) -> Result<(), String> {
    let value = name.check(value)?;
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

fn baseline(config: &Config, group: ListGroup, kind: ListKind) -> Vec<(String, EntrySource)> {
    let policy = policy_of(config, group);
    let mut entries: Vec<(String, EntrySource)> = Vec::new();
    let mut push = |value: &String, source| {
        if !entries
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(value))
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
        .any(|(known, _)| known.eq_ignore_ascii_case(value))
}

fn same_entry(entry: &StoredEntry, group: ListGroup, kind: ListKind, value: &str) -> bool {
    entry.group == group && entry.kind == kind && entry.value.eq_ignore_ascii_case(value)
}

pub fn compute(config: &Config, stored: &StoredSettings) -> Result<Effective, String> {
    let mut effective = config.clone();
    for value in &stored.values {
        override_value(&mut effective, value.name, value.value)?;
    }
    effective
        .validate_settings()
        .map_err(|error| format!("{error:#}"))?;
    let mut policies = Policies::from_config(&effective);
    for entry in &stored.entries {
        let policy = policy_mut(&mut policies, entry.group);
        let list = match entry.kind {
            ListKind::Logins => &mut policy.logins,
            ListKind::Tags => &mut policy.tags,
        };
        merge_logins(list, std::slice::from_ref(&entry.value));
    }
    Ok(Effective {
        config: effective,
        policies,
    })
}

impl Settings {
    pub fn load(config: &Config, db: &Db) -> anyhow::Result<Settings> {
        let loaded = db.load_settings()?;
        let mut stored = StoredSettings::default();
        for entry in loaded.entries {
            match entry.kind.check(&entry.value) {
                Ok(_) if !in_baseline(config, entry.group, entry.kind, &entry.value) => {
                    stored.entries.push(entry);
                }
                Ok(_) => {}
                Err(reason) => {
                    tracing::warn!(value = %entry.value, %reason, "ignoring stored access entry");
                }
            }
        }
        for value in loaded.values {
            stored.values.push(value.clone());
            if let Err(reason) = compute(config, &stored) {
                tracing::warn!(name = %value.name, %reason, "ignoring stored setting override");
                stored.values.pop();
            }
        }
        let effective = compute(config, &stored).map_err(anyhow::Error::msg)?;
        Ok(Settings {
            current: Mutex::new(Current {
                stored,
                effective: Arc::new(effective),
            }),
        })
    }

    pub fn effective(&self) -> Arc<Effective> {
        self.lock().effective.clone()
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Current> {
        self.current.lock().expect("settings mutex poisoned")
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

pub fn apply(
    config: &Config,
    db: &Db,
    current: &mut Current,
    change: Change,
    by: Option<&str>,
    now: DateTime<Utc>,
) -> anyhow::Result<Result<bool, Refusal>> {
    let by = by.map(str::to_owned);
    let mut stored = current.stored.clone();
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
                return Ok(Ok(false));
            }
            let entry = StoredEntry {
                group,
                kind,
                value,
                added_by: by,
                added_at: now,
            };
            stored.entries.push(entry.clone());
            let effective = match compute(config, &stored) {
                Ok(effective) => effective,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            db.add_entry(&entry)?;
            current.stored = stored;
            current.effective = Arc::new(effective);
        }
        Change::Remove(group, kind, value) => {
            if in_baseline(config, group, kind, &value) {
                return Ok(Err(Refusal::Fixed(format!(
                    "{value} comes from the server config"
                ))));
            }
            let before = stored.entries.len();
            stored
                .entries
                .retain(|entry| !same_entry(entry, group, kind, &value));
            if stored.entries.len() == before {
                return Ok(Err(Refusal::Missing(format!("{value} is not listed"))));
            }
            let effective = match compute(config, &stored) {
                Ok(effective) => effective,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            db.remove_entry(group, kind, &value)?;
            current.stored = stored;
            current.effective = Arc::new(effective);
        }
        Change::Set(name, value) => {
            let value = match name.check(value) {
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
            let effective = match compute(config, &stored) {
                Ok(effective) => effective,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            db.set_override(&stored_value)?;
            current.stored = stored;
            current.effective = Arc::new(effective);
        }
        Change::Reset(name) => {
            stored.values.retain(|known| known.name != name);
            let effective = match compute(config, &stored) {
                Ok(effective) => effective,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            db.remove_override(name)?;
            current.stored = stored;
            current.effective = Arc::new(effective);
        }
        Change::Approve(login) => {
            let known = in_baseline(config, ListGroup::Consumers, ListKind::Logins, &login)
                || stored
                    .entries
                    .iter()
                    .any(|entry| same_entry(entry, ListGroup::Consumers, ListKind::Logins, &login));
            let entry = match ListKind::Logins.check(&login) {
                Ok(login) if !known => Some(StoredEntry {
                    group: ListGroup::Consumers,
                    kind: ListKind::Logins,
                    value: login,
                    added_by: by,
                    added_at: now,
                }),
                Ok(_) => None,
                Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
            };
            let mut effective = None;
            if let Some(entry) = &entry {
                stored.entries.push(entry.clone());
                match compute(config, &stored) {
                    Ok(computed) => effective = Some(computed),
                    Err(reason) => return Ok(Err(Refusal::Invalid(reason))),
                }
            }
            if !db.approve_request(&login, entry.as_ref())? {
                return Ok(Err(Refusal::Missing(format!(
                    "no access request from {login}"
                ))));
            }
            if let Some(effective) = effective {
                current.stored = stored;
                current.effective = Arc::new(effective);
                return Ok(Ok(true));
            }
            return Ok(Ok(false));
        }
        Change::Deny(login) => {
            if !db.deny_request(&login, by.as_deref(), now)? {
                return Ok(Err(Refusal::Missing(format!(
                    "no access request from {login}"
                ))));
            }
            return Ok(Ok(false));
        }
        Change::Forget(login) => {
            if !db.forget_request(&login)? {
                return Ok(Err(Refusal::Missing(format!(
                    "no access request from {login}"
                ))));
            }
            return Ok(Ok(false));
        }
    }
    Ok(Ok(true))
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
                .any(|known| known.value.eq_ignore_ascii_case(&entry.value))
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

pub fn view(config: &Config, current: &Current, requests: Vec<AccessRequest>) -> SettingsView {
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
        current: &mut Current,
        change: Change,
    ) -> Result<bool, Refusal> {
        apply(config, db, current, change, Some("root@example.com"), t0()).unwrap()
    }

    #[test]
    fn effective_settings_keep_the_proxy_only_rules() {
        let config = proxy_only();
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&config, &db).unwrap();
        let mut current = settings.lock();
        let refused = change(
            &config,
            &db,
            &mut current,
            Change::Set(
                SettingName::ConsumersAllowLoopback,
                SettingValue::Flag(true),
            ),
        );
        assert!(
            matches!(refused, Err(Refusal::Invalid(reason)) if reason.contains("allow_loopback"))
        );
        assert!(!current.effective.config.consumers.allow_loopback);
        assert!(db.load_settings().unwrap().values.is_empty());
        assert_eq!(
            change(
                &config,
                &db,
                &mut current,
                Change::Set(SettingName::DaemonsAllowLoopback, SettingValue::Flag(false)),
            ),
            Ok(true)
        );
    }

    #[test]
    fn admins_are_consumers_and_the_baseline_stays() {
        let config = proxy_only();
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&config, &db).unwrap();
        let mut current = settings.lock();
        assert!(
            current
                .effective
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
                    &mut current,
                    Change::Remove(ListGroup::Consumers, ListKind::Logins, login.to_owned()),
                ),
                Err(Refusal::Fixed(_))
            ));
        }
        assert_eq!(
            change(
                &config,
                &db,
                &mut current,
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
    fn bad_stored_overrides_are_ignored_at_load() {
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
        let settings = Settings::load(&config, &db).unwrap();
        let effective = settings.effective();
        assert!(!effective.config.consumers.allow_loopback);
        assert_eq!(effective.config.max_attempts, 5);
        let open = Settings::load(
            &Config {
                web_dir: None,
                ..Config::default()
            },
            &db,
        )
        .unwrap();
        assert!(open.effective().config.consumers.allow_loopback);
    }
}
