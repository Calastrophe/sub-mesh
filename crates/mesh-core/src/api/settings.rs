use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SESSION_PATH: &str = "/api/v1/session";

pub const ACCESS_REQUEST_PATH: &str = "/api/v1/access-request";

pub const SETTINGS_PATH: &str = "/api/v1/settings";

pub const MAX_ACCESS_NOTE: usize = 200;

pub const MAX_ACCESS_REQUESTS: usize = 100;

pub const MAX_LOGIN_LEN: usize = 254;

const MAX_TAG_LEN: usize = 64;

const TAGGED_LOGIN: &str = "tagged-devices";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListGroup {
    Consumers,
    Daemons,
}

impl ListGroup {
    pub const ALL: [ListGroup; 2] = [ListGroup::Consumers, ListGroup::Daemons];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ListGroup::Consumers => "consumers",
            ListGroup::Daemons => "daemons",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListKind {
    Logins,
    Tags,
}

impl ListKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ListKind::Logins => "logins",
            ListKind::Tags => "tags",
        }
    }

    pub fn check(self, value: &str) -> Result<String, String> {
        match self {
            ListKind::Logins => check_login(value),
            ListKind::Tags => check_tag(value),
        }
    }
}

impl FromStr for ListKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "logins" => Ok(ListKind::Logins),
            "tags" => Ok(ListKind::Tags),
            _ => Err(format!("unknown list {s:?}")),
        }
    }
}

pub fn check_login(login: &str) -> Result<String, String> {
    let login = login.trim();
    if login.is_empty() {
        return Err("a login cannot be empty".to_owned());
    }
    if login.len() > MAX_LOGIN_LEN {
        return Err("the login is too long".to_owned());
    }
    if login.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("a login cannot contain spaces or control characters".to_owned());
    }
    if login.eq_ignore_ascii_case(TAGGED_LOGIN) {
        return Err("tagged-devices names no user".to_owned());
    }
    Ok(login.to_owned())
}

pub fn check_tag(tag: &str) -> Result<String, String> {
    let tag = tag.trim();
    let Some(name) = tag.strip_prefix("tag:") else {
        return Err("a tag starts with tag:".to_owned());
    };
    let valid = name.len() < MAX_TAG_LEN
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid {
        return Err(format!("{tag:?} is not a tag"));
    }
    Ok(tag.to_owned())
}

#[must_use]
pub fn entry_path(group: ListGroup, kind: ListKind, value: &str) -> String {
    format!(
        "{SETTINGS_PATH}/{}/{}/{}",
        group.as_str(),
        kind.as_str(),
        encode_segment(value)
    )
}

#[must_use]
pub fn setting_path(name: SettingName) -> String {
    format!("{SETTINGS_PATH}/values/{}", name.as_str())
}

#[must_use]
pub fn request_path(login: &str) -> String {
    format!("{SETTINGS_PATH}/requests/{}", encode_segment(login))
}

#[must_use]
pub fn decision_path(login: &str, approve: bool) -> String {
    let verb = if approve { "approve" } else { "deny" };
    format!("{}/{verb}", request_path(login))
}

#[must_use]
pub fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingName {
    CacheTtlSecs,
    AffinityMinHeadroom,
    MaxAttempts,
    DaemonStaleSecs,
    MaxBodyBytes,
    ConsumersAllowLoopback,
    DaemonsAllowLoopback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Seconds,
    Fraction,
    Count,
    Bytes,
    Flag,
}

impl SettingName {
    pub const ALL: [SettingName; 7] = [
        SettingName::CacheTtlSecs,
        SettingName::AffinityMinHeadroom,
        SettingName::MaxAttempts,
        SettingName::DaemonStaleSecs,
        SettingName::MaxBodyBytes,
        SettingName::ConsumersAllowLoopback,
        SettingName::DaemonsAllowLoopback,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SettingName::CacheTtlSecs => "cache_ttl_secs",
            SettingName::AffinityMinHeadroom => "affinity_min_headroom",
            SettingName::MaxAttempts => "max_attempts",
            SettingName::DaemonStaleSecs => "daemon_stale_secs",
            SettingName::MaxBodyBytes => "max_body_bytes",
            SettingName::ConsumersAllowLoopback => "consumers.allow_loopback",
            SettingName::DaemonsAllowLoopback => "daemons.allow_loopback",
        }
    }

    #[must_use]
    pub fn kind(self) -> SettingKind {
        match self {
            SettingName::CacheTtlSecs | SettingName::DaemonStaleSecs => SettingKind::Seconds,
            SettingName::AffinityMinHeadroom => SettingKind::Fraction,
            SettingName::MaxAttempts => SettingKind::Count,
            SettingName::MaxBodyBytes => SettingKind::Bytes,
            SettingName::ConsumersAllowLoopback | SettingName::DaemonsAllowLoopback => {
                SettingKind::Flag
            }
        }
    }

    #[must_use]
    pub fn bounds(self) -> (f64, f64) {
        match self {
            SettingName::CacheTtlSecs | SettingName::DaemonStaleSecs => (1.0, 86_400.0),
            SettingName::AffinityMinHeadroom => (0.0, 1.0),
            SettingName::MaxAttempts => (1.0, 10.0),
            SettingName::MaxBodyBytes => (1_048_576.0, 536_870_912.0),
            SettingName::ConsumersAllowLoopback | SettingName::DaemonsAllowLoopback => (0.0, 1.0),
        }
    }

    pub fn check(self, value: SettingValue) -> Result<SettingValue, String> {
        let name = self.as_str();
        if self.kind() == SettingKind::Flag {
            return match value {
                SettingValue::Flag(flag) => Ok(SettingValue::Flag(flag)),
                _ => Err(format!("{name} is true or false")),
            };
        }
        let number = match value {
            SettingValue::Flag(_) => return Err(format!("{name} is a number")),
            SettingValue::Whole(whole) => whole as f64,
            SettingValue::Fraction(fraction) => fraction,
        };
        let (low, high) = self.bounds();
        if !number.is_finite() || number < low || number > high {
            return Err(format!("{name} must be between {low} and {high}"));
        }
        if self.kind() == SettingKind::Fraction {
            return Ok(SettingValue::Fraction(number));
        }
        if number.fract() != 0.0 {
            return Err(format!("{name} must be a whole number"));
        }
        Ok(SettingValue::Whole(number as u64))
    }
}

impl fmt::Display for SettingName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SettingName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SettingName::ALL
            .into_iter()
            .find(|name| name.as_str() == s)
            .ok_or_else(|| format!("unknown setting {s:?}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    Flag(bool),
    Whole(u64),
    Fraction(f64),
}

impl SettingValue {
    #[must_use]
    pub fn as_f64(self) -> f64 {
        match self {
            SettingValue::Flag(flag) => f64::from(u8::from(flag)),
            SettingValue::Whole(whole) => whole as f64,
            SettingValue::Fraction(fraction) => fraction,
        }
    }

    #[must_use]
    pub fn as_flag(self) -> bool {
        matches!(self, SettingValue::Flag(true))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSetting {
    pub value: SettingValue,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestAccess {
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Pending,
    Denied,
}

impl RequestState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RequestState::Pending => "pending",
            RequestState::Denied => "denied",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRequest {
    pub login: String,
    pub note: Option<String>,
    pub state: RequestState,
    pub requested_at: DateTime<Utc>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub login: Option<String>,
    pub node: String,
    pub consumer: bool,
    pub admin: bool,
    pub request: Option<AccessRequest>,
    pub pending_requests: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrySource {
    Config,
    Admins,
    Web,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    pub value: String,
    pub source: EntrySource,
    pub added_by: Option<String>,
    pub added_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessList {
    pub logins: Vec<ListEntry>,
    pub tags: Vec<ListEntry>,
    pub own_login: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tunable {
    pub name: SettingName,
    pub value: SettingValue,
    pub config: SettingValue,
    pub overridden: bool,
    pub set_by: Option<String>,
    pub set_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyInfo {
    pub addrs: Vec<String>,
    pub header: String,
    pub only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedSettings {
    pub listen: String,
    pub db_path: String,
    pub web_dir: Option<String>,
    pub tailscale_socket: String,
    pub admins: Vec<String>,
    pub trusted_proxy: Option<ProxyInfo>,
    pub insecure_skip_whois: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub consumers: AccessList,
    pub daemons: AccessList,
    pub values: Vec<Tunable>,
    pub requests: Vec<AccessRequest>,
    pub fixed: FixedSettings,
}

impl SettingsView {
    #[must_use]
    pub fn list(&self, group: ListGroup) -> &AccessList {
        match group {
            ListGroup::Consumers => &self.consumers,
            ListGroup::Daemons => &self.daemons,
        }
    }

    #[must_use]
    pub fn pending(&self) -> usize {
        self.requests
            .iter()
            .filter(|request| request.state == RequestState::Pending)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_encode_their_values() {
        assert_eq!(
            entry_path(ListGroup::Consumers, ListKind::Logins, "a b/c@x"),
            "/api/v1/settings/consumers/logins/a%20b%2Fc@x"
        );
        assert_eq!(
            entry_path(ListGroup::Daemons, ListKind::Tags, "tag:mesh"),
            "/api/v1/settings/daemons/tags/tag%3Amesh"
        );
        assert_eq!(
            setting_path(SettingName::ConsumersAllowLoopback),
            "/api/v1/settings/values/consumers.allow_loopback"
        );
        assert_eq!(
            decision_path("vablings@github", true),
            "/api/v1/settings/requests/vablings@github/approve"
        );
        assert!(SETTINGS_PATH.starts_with(super::super::PREFIX));
    }

    #[test]
    fn logins_and_tags_are_checked() {
        assert_eq!(check_login(" a@b ").unwrap(), "a@b");
        for bad in [
            "",
            "  ",
            "a b",
            "tagged-devices",
            "Tagged-Devices",
            "a\u{7}",
        ] {
            assert!(check_login(bad).is_err(), "{bad:?}");
        }
        assert!(check_login(&"a".repeat(MAX_LOGIN_LEN + 1)).is_err());
        assert_eq!(check_tag("tag:mesh-1").unwrap(), "tag:mesh-1");
        for bad in ["mesh", "tag:", "tag:1a", "tag:a b", "tag:a_b"] {
            assert!(check_tag(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn values_follow_their_kind() {
        let check = |name: SettingName, json: &str| {
            let value: SettingValue = serde_json::from_str(json).unwrap();
            name.check(value)
        };
        assert_eq!(
            check(SettingName::CacheTtlSecs, "600"),
            Ok(SettingValue::Whole(600))
        );
        assert_eq!(
            check(SettingName::AffinityMinHeadroom, "0.05"),
            Ok(SettingValue::Fraction(0.05))
        );
        assert_eq!(
            check(SettingName::AffinityMinHeadroom, "1"),
            Ok(SettingValue::Fraction(1.0))
        );
        assert_eq!(
            check(SettingName::ConsumersAllowLoopback, "true"),
            Ok(SettingValue::Flag(true))
        );
        for (name, json) in [
            (SettingName::CacheTtlSecs, "0"),
            (SettingName::CacheTtlSecs, "1.5"),
            (SettingName::CacheTtlSecs, "true"),
            (SettingName::AffinityMinHeadroom, "1.5"),
            (SettingName::MaxAttempts, "11"),
            (SettingName::MaxBodyBytes, "10"),
            (SettingName::DaemonsAllowLoopback, "1"),
        ] {
            assert!(check(name, json).is_err(), "{name} {json}");
        }
        for name in SettingName::ALL {
            assert_eq!(name.as_str().parse::<SettingName>(), Ok(name));
        }
    }
}
