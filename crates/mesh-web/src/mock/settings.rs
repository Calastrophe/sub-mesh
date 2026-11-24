use std::cell::RefCell;

use chrono::{Duration, Utc};
use mesh_core::api::{
    AccessList, AccessRequest, EntrySource, FixedSettings, ListEntry, ListGroup, ListKind,
    ProxyInfo, RequestState, Session, SettingName, SettingValue, SettingsView, Tunable,
};

use super::{BOB, CAROL, ME};
use crate::api::{Decision, Error, Result};

const GUEST: &str = "guest@example.com";
const DAVE: &str = "dave@example.com";
const ERIN: &str = "erin@example.com";
const FRANK: &str = "frank@example.com";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visitor {
    Admin,
    Guest(Option<RequestState>),
}

pub fn visitor_from(search: &str) -> Visitor {
    let guest =
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| match pair.split_once('=') {
                Some(("guest", value)) => Some(Some(value)),
                None if pair == "guest" => Some(None),
                _ => None,
            });
    match guest {
        None => Visitor::Admin,
        Some(Some("pending")) => Visitor::Guest(Some(RequestState::Pending)),
        Some(Some("denied")) => Visitor::Guest(Some(RequestState::Denied)),
        Some(_) => Visitor::Guest(None),
    }
}

fn visitor() -> Visitor {
    thread_local! {
        static VISITOR: Visitor = if cfg!(target_arch = "wasm32") {
            web_sys::window()
                .and_then(|window| window.location().search().ok())
                .map_or(Visitor::Admin, |search| visitor_from(&search))
        } else {
            Visitor::Admin
        };
    }
    VISITOR.with(|visitor| *visitor)
}

thread_local! {
    static SETTINGS: RefCell<SettingsView> = RefCell::new(sample_settings());
    static GUEST_REQUEST: RefCell<Option<AccessRequest>> = RefCell::new(match visitor() {
        Visitor::Guest(Some(state)) => Some(guest_request(state, Some("Lab laptop".to_owned()))),
        _ => None,
    });
}

fn guest_request(state: RequestState, note: Option<String>) -> AccessRequest {
    let now = Utc::now();
    AccessRequest {
        login: GUEST.to_owned(),
        note,
        state,
        requested_at: now - Duration::hours(2),
        decided_by: (state == RequestState::Denied).then(|| ME.to_owned()),
        decided_at: (state == RequestState::Denied).then(|| now - Duration::minutes(30)),
    }
}

fn api_error(status: u16, message: impl Into<String>) -> Error {
    Error::Api {
        status,
        message: message.into(),
    }
}

pub fn session() -> Result<Session> {
    Ok(match visitor() {
        Visitor::Admin => Session {
            login: Some(ME.to_owned()),
            node: "bb-1".to_owned(),
            consumer: true,
            admin: true,
            request: None,
            pending_requests: Some(SETTINGS.with_borrow(|view| view.pending() as u32)),
        },
        Visitor::Guest(_) => Session {
            login: Some(GUEST.to_owned()),
            node: "guest-laptop".to_owned(),
            consumer: false,
            admin: false,
            request: GUEST_REQUEST.with_borrow(Clone::clone),
            pending_requests: None,
        },
    })
}

pub fn request_access(note: Option<String>) -> Result<Session> {
    if visitor() == Visitor::Admin {
        return Err(api_error(409, "you already have access"));
    }
    let note = note
        .map(|note| note.trim().to_owned())
        .filter(|note| !note.is_empty());
    if note
        .as_ref()
        .is_some_and(|note| note.chars().count() > mesh_core::api::MAX_ACCESS_NOTE)
    {
        return Err(api_error(400, "the note is too long"));
    }
    GUEST_REQUEST.with_borrow_mut(|request| {
        if request.is_none() {
            *request = Some(AccessRequest {
                requested_at: Utc::now(),
                ..guest_request(RequestState::Pending, note)
            });
        }
    });
    session()
}

fn admin_only() -> Result<()> {
    match visitor() {
        Visitor::Admin => Ok(()),
        Visitor::Guest(_) => Err(api_error(403, "forbidden")),
    }
}

pub fn settings() -> Result<SettingsView> {
    admin_only()?;
    Ok(SETTINGS.with_borrow(Clone::clone))
}

fn list_mut(view: &mut SettingsView, group: ListGroup, kind: ListKind) -> &mut Vec<ListEntry> {
    let list = match group {
        ListGroup::Consumers => &mut view.consumers,
        ListGroup::Daemons => &mut view.daemons,
    };
    match kind {
        ListKind::Logins => &mut list.logins,
        ListKind::Tags => &mut list.tags,
    }
}

fn web_entry(value: &str, ago: Duration) -> ListEntry {
    ListEntry {
        value: value.to_owned(),
        source: EntrySource::Web,
        added_by: Some(ME.to_owned()),
        added_at: Some(Utc::now() - ago),
    }
}

pub fn set_entry(group: ListGroup, kind: ListKind, value: &str, add: bool) -> Result<SettingsView> {
    admin_only()?;
    let value = kind.check(value).map_err(|reason| api_error(400, reason))?;
    SETTINGS.with_borrow_mut(|view| {
        if add && group == ListGroup::Consumers && kind == ListKind::Logins {
            view.requests
                .retain(|request| !request.login.eq_ignore_ascii_case(&value));
        }
        let list = list_mut(view, group, kind);
        let index = list
            .iter()
            .position(|entry| entry.value.eq_ignore_ascii_case(&value));
        match (add, index) {
            (true, Some(_)) => {}
            (true, None) => list.push(web_entry(&value, Duration::zero())),
            (false, Some(index)) if list[index].source == EntrySource::Web => {
                list.remove(index);
            }
            (false, Some(_)) => {
                return Err(api_error(
                    409,
                    format!("{value} comes from the server config"),
                ));
            }
            (false, None) => return Err(api_error(404, format!("{value} is not listed"))),
        }
        Ok(view.clone())
    })
}

pub fn set_value(name: SettingName, value: Option<SettingValue>) -> Result<SettingsView> {
    admin_only()?;
    let value = value
        .map(|value| name.check(value).map_err(|reason| api_error(400, reason)))
        .transpose()?;
    SETTINGS.with_borrow_mut(|view| {
        let proxy_only = view
            .fixed
            .trusted_proxy
            .as_ref()
            .is_some_and(|proxy| proxy.only);
        let loopback = matches!(
            name,
            SettingName::ConsumersAllowLoopback | SettingName::DaemonsAllowLoopback
        );
        if proxy_only && loopback && value.is_some_and(SettingValue::as_flag) {
            return Err(api_error(
                400,
                "trusted_proxy.only cannot be combined with allow_loopback",
            ));
        }
        let tunable = view
            .values
            .iter_mut()
            .find(|tunable| tunable.name == name)
            .ok_or_else(|| api_error(404, format!("unknown setting {name}")))?;
        match value {
            Some(value) => {
                tunable.value = value;
                tunable.overridden = true;
                tunable.set_by = Some(ME.to_owned());
                tunable.set_at = Some(Utc::now());
            }
            None => {
                tunable.value = tunable.config;
                tunable.overridden = false;
                tunable.set_by = None;
                tunable.set_at = None;
            }
        }
        Ok(view.clone())
    })
}

pub fn decide(login: &str, decision: Decision) -> Result<SettingsView> {
    admin_only()?;
    SETTINGS.with_borrow_mut(|view| {
        let index = view
            .requests
            .iter()
            .position(|request| request.login.eq_ignore_ascii_case(login))
            .ok_or_else(|| api_error(404, format!("no access request from {login}")))?;
        match decision {
            Decision::Approve => {
                let request = view.requests.remove(index);
                let logins = &mut view.consumers.logins;
                if !logins
                    .iter()
                    .any(|entry| entry.value.eq_ignore_ascii_case(&request.login))
                {
                    logins.push(web_entry(&request.login, Duration::zero()));
                }
            }
            Decision::Deny => {
                let request = &mut view.requests[index];
                request.state = RequestState::Denied;
                request.decided_by = Some(ME.to_owned());
                request.decided_at = Some(Utc::now());
            }
            Decision::Forget => {
                view.requests.remove(index);
            }
        }
        Ok(view.clone())
    })
}

fn config_entry(value: &str, source: EntrySource) -> ListEntry {
    ListEntry {
        value: value.to_owned(),
        source,
        added_by: None,
        added_at: None,
    }
}

fn tunable(name: SettingName, config: SettingValue, value: Option<SettingValue>) -> Tunable {
    Tunable {
        name,
        value: value.unwrap_or(config),
        config,
        overridden: value.is_some(),
        set_by: value.map(|_| ME.to_owned()),
        set_at: value.map(|_| Utc::now() - Duration::days(2)),
    }
}

fn sample_settings() -> SettingsView {
    let now = Utc::now();
    let request = |login: &str, note: Option<&str>, ago: Duration, state| AccessRequest {
        login: login.to_owned(),
        note: note.map(str::to_owned),
        state,
        requested_at: now - ago,
        decided_by: (state == RequestState::Denied).then(|| ME.to_owned()),
        decided_at: (state == RequestState::Denied).then(|| now - ago + Duration::hours(1)),
    };
    SettingsView {
        consumers: AccessList {
            logins: vec![
                config_entry(BOB, EntrySource::Config),
                config_entry(ME, EntrySource::Admins),
                web_entry(CAROL, Duration::days(3)),
            ],
            tags: vec![config_entry("tag:ci", EntrySource::Config)],
            own_login: false,
        },
        daemons: AccessList {
            logins: vec![
                config_entry(ME, EntrySource::Config),
                config_entry(BOB, EntrySource::Config),
                web_entry(CAROL, Duration::days(3)),
            ],
            tags: vec![web_entry("tag:mesh", Duration::days(9))],
            own_login: false,
        },
        values: vec![
            tunable(SettingName::CacheTtlSecs, SettingValue::Whole(300), None),
            tunable(
                SettingName::AffinityMinHeadroom,
                SettingValue::Fraction(0.02),
                Some(SettingValue::Fraction(0.05)),
            ),
            tunable(SettingName::MaxAttempts, SettingValue::Whole(3), None),
            tunable(SettingName::DaemonStaleSecs, SettingValue::Whole(60), None),
            tunable(
                SettingName::MaxBodyBytes,
                SettingValue::Whole(64 * 1024 * 1024),
                None,
            ),
            tunable(
                SettingName::ConsumersAllowLoopback,
                SettingValue::Flag(false),
                None,
            ),
            tunable(
                SettingName::DaemonsAllowLoopback,
                SettingValue::Flag(false),
                None,
            ),
        ],
        requests: vec![
            request(
                DAVE,
                Some("Lab cluster, Claude only"),
                Duration::minutes(40),
                RequestState::Pending,
            ),
            request(ERIN, None, Duration::hours(5), RequestState::Pending),
            request(FRANK, Some("test"), Duration::days(2), RequestState::Denied),
        ],
        fixed: FixedSettings {
            listen: "10.10.10.102:7070".to_owned(),
            db_path: "/var/lib/mesh/server.db".to_owned(),
            web_dir: Some("/usr/share/mesh/web".to_owned()),
            tailscale_socket: "/var/run/tailscale/tailscaled.sock".to_owned(),
            admins: vec![ME.to_owned()],
            trusted_proxy: Some(ProxyInfo {
                addrs: vec!["10.10.10.111".to_owned()],
                header: "x-webauth-user".to_owned(),
                only: true,
            }),
            insecure_skip_whois: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_query_picks_the_visitor() {
        assert_eq!(visitor_from("?mock"), Visitor::Admin);
        assert_eq!(visitor_from("?mock&guest"), Visitor::Guest(None));
        assert_eq!(
            visitor_from("?mock&guest=pending"),
            Visitor::Guest(Some(RequestState::Pending))
        );
        assert_eq!(
            visitor_from("?guest=denied&mock"),
            Visitor::Guest(Some(RequestState::Denied))
        );
        assert_eq!(visitor_from("?mock&guests"), Visitor::Admin);
    }

    #[test]
    fn settings_changes_follow_the_server_rules() {
        SETTINGS.with_borrow_mut(|view| *view = sample_settings());
        let added = set_entry(ListGroup::Consumers, ListKind::Logins, DAVE, true).unwrap();
        assert!(added.requests.iter().all(|request| request.login != DAVE));
        assert!(matches!(
            set_entry(ListGroup::Consumers, ListKind::Logins, BOB, false),
            Err(Error::Api { status: 409, .. })
        ));
        assert!(matches!(
            set_entry(ListGroup::Daemons, ListKind::Tags, "mesh", true),
            Err(Error::Api { status: 400, .. })
        ));
        let approved = decide(ERIN, Decision::Approve).unwrap();
        assert!(
            approved
                .consumers
                .logins
                .iter()
                .any(|entry| entry.value == ERIN)
        );
        assert_eq!(approved.pending(), 0);
        let reset = set_value(SettingName::AffinityMinHeadroom, None).unwrap();
        let headroom = reset
            .values
            .iter()
            .find(|tunable| tunable.name == SettingName::AffinityMinHeadroom)
            .unwrap();
        assert!(!headroom.overridden);
        assert!(matches!(
            set_value(
                SettingName::ConsumersAllowLoopback,
                Some(SettingValue::Flag(true))
            ),
            Err(Error::Api { status: 400, .. })
        ));
    }
}
