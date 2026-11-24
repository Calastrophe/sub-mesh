use std::net::IpAddr;

use mesh_core::api::{
    ACCESS_REQUEST_PATH, EntrySource, ListGroup, ListKind, OVERVIEW_PATH, RequestState,
    SESSION_PATH, SETTINGS_PATH, Session, SettingName, SettingValue, SettingsView, decision_path,
    entry_path, request_path, setting_path,
};
use mesh_core::protocol::REPORT_PATH;
use serde_json::json;

use super::{Arc, SocketAddr, account, report, serve};
use crate::AppState;
use crate::config::{Admins, Config, PeerPolicy, TrustedProxy};
#[cfg(unix)]
use crate::testing::fake_tailnet;
use crate::testing::{PROXY_HEADER, TempDir, state_exact};

const ROOT: &str = "root@example.com";
const ALICE: &str = "alice@example.com";
const CAROL: &str = "carol@example.com";
const TAGGED: &str = "tagged-devices";

fn proxy_config() -> Config {
    Config {
        consumers: PeerPolicy {
            logins: vec![ALICE.to_owned()],
            ..PeerPolicy::default()
        },
        daemons: PeerPolicy {
            logins: vec![ALICE.to_owned()],
            ..PeerPolicy::default()
        },
        admins: Admins {
            logins: vec![ROOT.to_owned()],
        },
        trusted_proxy: TrustedProxy {
            addrs: vec!["127.0.0.1".parse().unwrap()],
            only: true,
            ..TrustedProxy::default()
        },
        tailscale_socket: "/nonexistent/tailscaled.sock".into(),
        web_dir: None,
        ..Config::default()
    }
}

#[derive(Clone)]
struct As {
    client: reqwest::Client,
    base: String,
    header: Option<&'static str>,
}

impl As {
    fn proxied(addr: SocketAddr, login: Option<&'static str>) -> As {
        As {
            client: reqwest::Client::new(),
            base: format!("http://{addr}"),
            header: login,
        }
    }

    fn from(addr: SocketAddr, ip: &str) -> As {
        As {
            client: reqwest::Client::builder()
                .local_address(ip.parse::<IpAddr>().unwrap())
                .build()
                .unwrap(),
            base: format!("http://{addr}"),
            header: None,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.client.request(method, format!("{}{path}", self.base));
        match self.header {
            Some(login) => request.header(PROXY_HEADER, login),
            None => request,
        }
    }

    async fn status(&self, method: reqwest::Method, path: &str) -> u16 {
        self.request(method, path)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn session(&self) -> Result<Session, u16> {
        let response = self
            .request(reqwest::Method::GET, SESSION_PATH)
            .send()
            .await
            .unwrap();
        match response.status().as_u16() {
            200 => Ok(response.json().await.unwrap()),
            status => Err(status),
        }
    }

    async fn ask(&self, note: Option<&str>) -> Result<Session, u16> {
        let response = self
            .request(reqwest::Method::POST, ACCESS_REQUEST_PATH)
            .json(&json!({ "note": note }))
            .send()
            .await
            .unwrap();
        match response.status().as_u16() {
            200 => Ok(response.json().await.unwrap()),
            status => Err(status),
        }
    }

    async fn settings(&self, method: reqwest::Method, path: &str) -> Result<SettingsView, u16> {
        self.settings_with(self.request(method, path)).await
    }

    async fn settings_with(&self, request: reqwest::RequestBuilder) -> Result<SettingsView, u16> {
        let response = request.send().await.unwrap();
        match response.status().as_u16() {
            200 => Ok(response.json().await.unwrap()),
            status => Err(status),
        }
    }

    async fn set(&self, name: SettingName, value: serde_json::Value) -> Result<SettingsView, u16> {
        self.settings_with(
            self.request(reqwest::Method::PUT, &setting_path(name))
                .json(&json!({ "value": value })),
        )
        .await
    }

    async fn report(&self, node: &str) -> u16 {
        self.request(reqwest::Method::POST, REPORT_PATH)
            .json(&report(node, vec![account(&format!("{node}-acct"), 0.5)]))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn relay(&self) -> u16 {
        self.request(reqwest::Method::POST, "/codex/responses")
            .body("payload")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

const GET: reqwest::Method = reqwest::Method::GET;
const PUT: reqwest::Method = reqwest::Method::PUT;
const POST: reqwest::Method = reqwest::Method::POST;
const DELETE: reqwest::Method = reqwest::Method::DELETE;

fn consumer_login(value: &str) -> String {
    entry_path(ListGroup::Consumers, ListKind::Logins, value)
}

fn daemon_login(value: &str) -> String {
    entry_path(ListGroup::Daemons, ListKind::Logins, value)
}

fn sources(view: &SettingsView, group: ListGroup) -> Vec<(String, EntrySource)> {
    view.list(group)
        .logins
        .iter()
        .map(|entry| (entry.value.clone(), entry.source))
        .collect()
}

async fn proxied() -> (Arc<AppState>, SocketAddr) {
    let state = state_exact(proxy_config());
    let addr = serve(state.clone()).await;
    (state, addr)
}

#[tokio::test]
async fn only_admins_see_and_change_settings() {
    let (_state, addr) = proxied().await;
    let root = As::proxied(addr, Some(ROOT));
    let alice = As::proxied(addr, Some(ALICE));
    let carol = As::proxied(addr, Some(CAROL));

    let session = root.session().await.unwrap();
    assert!(session.consumer && session.admin);
    assert_eq!(session.pending_requests, Some(0));
    let session = alice.session().await.unwrap();
    assert!(session.consumer && !session.admin);
    assert_eq!(session.pending_requests, None);

    let view = root.settings(GET, SETTINGS_PATH).await.unwrap();
    assert_eq!(
        sources(&view, ListGroup::Consumers),
        [
            (ALICE.to_owned(), EntrySource::Config),
            (ROOT.to_owned(), EntrySource::Admins)
        ]
    );
    assert_eq!(view.fixed.admins, [ROOT]);
    assert!(
        view.fixed
            .trusted_proxy
            .as_ref()
            .is_some_and(|proxy| proxy.only)
    );

    for caller in [&alice, &carol] {
        assert_eq!(caller.settings(GET, SETTINGS_PATH).await, Err(403));
        assert_eq!(
            caller.settings(PUT, &consumer_login("x@example.com")).await,
            Err(403)
        );
        assert_eq!(
            caller.set(SettingName::MaxAttempts, json!(5)).await,
            Err(403)
        );
        assert_eq!(
            caller.settings(POST, &decision_path(CAROL, true)).await,
            Err(403)
        );
    }
    let cross_site = root
        .request(PUT, &consumer_login("x@example.com"))
        .header("origin", "http://evil.example")
        .header("host", addr.to_string());
    assert_eq!(root.settings_with(cross_site).await, Err(403));
    let view = root.settings(GET, SETTINGS_PATH).await.unwrap();
    assert_eq!(view.consumers.logins.len(), 2);
}

#[tokio::test]
async fn web_managed_logins_apply_at_once_and_survive_a_restart() {
    let dir = TempDir::new();
    let config = Config {
        db_path: dir.path().join("server.db"),
        ..proxy_config()
    };
    let state = crate::build_state(config.clone()).unwrap();
    let addr = serve(state.clone()).await;
    let root = As::proxied(addr, Some(ROOT));
    let carol = As::proxied(addr, Some(CAROL));

    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 403);
    assert_eq!(carol.report("carol-box").await, 403);
    let view = root.settings(PUT, &consumer_login(CAROL)).await.unwrap();
    assert!(sources(&view, ListGroup::Consumers).contains(&(CAROL.to_owned(), EntrySource::Web)));
    let entry = view
        .consumers
        .logins
        .iter()
        .find(|e| e.value == CAROL)
        .unwrap();
    assert_eq!(entry.added_by.as_deref(), Some(ROOT));
    root.settings(PUT, &daemon_login(CAROL)).await.unwrap();
    root.settings(
        PUT,
        &entry_path(ListGroup::Daemons, ListKind::Tags, "tag:mesh"),
    )
    .await
    .unwrap();
    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 200);
    assert_eq!(carol.report("carol-box").await, 204);
    assert!(carol.session().await.unwrap().consumer);

    let again = root
        .settings(PUT, &consumer_login("Carol@Example.com"))
        .await
        .unwrap();
    assert_eq!(again.consumers.logins.len(), 3);

    let restarted = crate::build_state(config.clone()).unwrap();
    let addr = serve(restarted.clone()).await;
    let root = As::proxied(addr, Some(ROOT));
    let carol = As::proxied(addr, Some(CAROL));
    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 200);
    assert_eq!(carol.report("carol-box").await, 204);
    let view = root.settings(GET, SETTINGS_PATH).await.unwrap();
    assert_eq!(view.daemons.tags.len(), 1);

    assert_eq!(
        root.settings(DELETE, &consumer_login(CAROL))
            .await
            .map(|_| ()),
        Ok(())
    );
    assert_eq!(
        root.settings(DELETE, &daemon_login("carol@EXAMPLE.com"))
            .await
            .map(|_| ()),
        Ok(())
    );
    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 403);
    assert_eq!(carol.report("carol-box").await, 403);
    assert_eq!(
        root.settings(DELETE, &consumer_login(CAROL)).await,
        Err(404)
    );

    let restarted = crate::build_state(config).unwrap();
    let addr = serve(restarted).await;
    assert_eq!(
        As::proxied(addr, Some(CAROL))
            .status(GET, OVERVIEW_PATH)
            .await,
        403
    );
}

#[tokio::test]
async fn config_entries_cannot_be_removed_from_the_web() {
    let (_state, addr) = proxied().await;
    let root = As::proxied(addr, Some(ROOT));
    for path in [
        consumer_login(ALICE),
        consumer_login(ROOT),
        consumer_login("Alice@Example.com"),
        daemon_login(ALICE),
    ] {
        assert_eq!(root.settings(DELETE, &path).await, Err(409), "{path}");
    }
    for path in [
        consumer_login(TAGGED),
        consumer_login("two words"),
        entry_path(ListGroup::Daemons, ListKind::Tags, "mesh"),
        format!("{SETTINGS_PATH}/consumers/groups/x"),
    ] {
        let status = root.settings(PUT, &path).await.unwrap_err();
        assert!(status == 400 || status == 404, "{path}: {status}");
    }
    assert_eq!(
        As::proxied(addr, Some(ALICE))
            .status(GET, OVERVIEW_PATH)
            .await,
        200
    );
}

#[tokio::test]
async fn an_approved_request_admits_the_caller() {
    let (state, addr) = proxied().await;
    let root = As::proxied(addr, Some(ROOT));
    let carol = As::proxied(addr, Some(CAROL));

    let session = carol.session().await.unwrap();
    assert!(!session.consumer && !session.admin);
    assert_eq!(session.login.as_deref(), Some(CAROL));
    assert_eq!(session.request, None);
    for path in [
        OVERVIEW_PATH,
        "/api/v1/logs",
        "/api/v1/usage",
        SETTINGS_PATH,
    ] {
        assert_eq!(carol.status(GET, path).await, 403, "{path}");
    }
    assert_eq!(carol.relay().await, 403);

    let first = carol.ask(Some("  from the lab  ")).await.unwrap();
    let request = first.request.clone().unwrap();
    assert_eq!(request.state, RequestState::Pending);
    assert_eq!(request.note.as_deref(), Some("from the lab"));
    let second = carol.ask(Some("other words")).await.unwrap();
    assert_eq!(second.request, Some(request.clone()));
    assert_eq!(carol.session().await.unwrap().request, Some(request));
    assert_eq!(root.session().await.unwrap().pending_requests, Some(1));
    assert_eq!(
        root.settings(GET, SETTINGS_PATH)
            .await
            .unwrap()
            .requests
            .len(),
        1
    );
    assert_eq!(carol.relay().await, 403);

    let view = root
        .settings(POST, &decision_path(CAROL, true))
        .await
        .unwrap();
    assert!(view.requests.is_empty());
    assert!(sources(&view, ListGroup::Consumers).contains(&(CAROL.to_owned(), EntrySource::Web)));
    let session = carol.session().await.unwrap();
    assert!(session.consumer);
    assert_eq!(session.request, None);
    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 200);
    assert_ne!(carol.relay().await, 403);
    assert_eq!(carol.ask(None).await, Err(409));
    assert_eq!(
        root.settings(POST, &decision_path(CAROL, true)).await,
        Err(404)
    );
    assert!(
        state
            .peers
            .policies()
            .consumers
            .logins
            .contains(&CAROL.to_owned())
    );
}

#[tokio::test]
async fn a_denied_request_stays_denied() {
    let (_state, addr) = proxied().await;
    let root = As::proxied(addr, Some(ROOT));
    let dave = As::proxied(addr, Some("dave@example.com"));
    dave.ask(None).await.unwrap();
    root.settings(POST, &decision_path("dave@example.com", false))
        .await
        .unwrap();
    let denied = dave.session().await.unwrap().request.unwrap();
    assert_eq!(denied.state, RequestState::Denied);
    assert_eq!(denied.decided_by.as_deref(), Some(ROOT));
    assert_eq!(
        dave.ask(Some("please")).await.unwrap().request,
        Some(denied)
    );
    assert_eq!(dave.status(GET, OVERVIEW_PATH).await, 403);
    assert_eq!(root.session().await.unwrap().pending_requests, Some(0));

    root.settings(DELETE, &request_path("dave@example.com"))
        .await
        .unwrap();
    assert_eq!(dave.session().await.unwrap().request, None);
    let again = dave.ask(None).await.unwrap().request.unwrap();
    assert_eq!(again.state, RequestState::Pending);
}

#[tokio::test]
async fn requests_need_a_plain_login_and_a_short_note() {
    let (_state, addr) = proxied().await;
    for caller in [As::proxied(addr, None), As::proxied(addr, Some(TAGGED))] {
        assert_eq!(caller.session().await, Err(403));
        assert_eq!(caller.ask(None).await, Err(403));
    }
    let erin = As::proxied(addr, Some("erin@example.com"));
    let long = "x".repeat(mesh_core::api::MAX_ACCESS_NOTE + 1);
    assert_eq!(erin.ask(Some(&long)).await, Err(400));
    assert_eq!(erin.ask(Some("bell\u{7}")).await, Err(400));
    let cross_site = erin
        .request(POST, ACCESS_REQUEST_PATH)
        .header("sec-fetch-site", "cross-site")
        .json(&json!({}));
    assert_eq!(cross_site.send().await.unwrap().status(), 403);
    assert_eq!(erin.session().await.unwrap().request, None);
    let huge = erin
        .request(POST, ACCESS_REQUEST_PATH)
        .header("content-type", "application/json")
        .body(format!("{{\"note\":\"{}\"}}", "y".repeat(10_000)));
    assert_eq!(huge.send().await.unwrap().status(), 413);
    assert!(erin.ask(None).await.unwrap().request.is_some());
}

#[tokio::test]
async fn scalar_overrides_apply_and_reset() {
    let (state, addr) = proxied().await;
    let root = As::proxied(addr, Some(ROOT));
    let view = root.set(SettingName::MaxAttempts, json!(5)).await.unwrap();
    let tunable = view
        .values
        .iter()
        .find(|tunable| tunable.name == SettingName::MaxAttempts)
        .unwrap();
    assert_eq!(tunable.value, SettingValue::Whole(5));
    assert_eq!(tunable.config, SettingValue::Whole(3));
    assert!(tunable.overridden);
    assert_eq!(tunable.set_by.as_deref(), Some(ROOT));
    assert_eq!(state.tuning().config.max_attempts, 5);

    root.set(SettingName::AffinityMinHeadroom, json!(0.25))
        .await
        .unwrap();
    root.set(SettingName::CacheTtlSecs, json!(900))
        .await
        .unwrap();
    assert_eq!(state.tuning().config.affinity_min_headroom, 0.25);
    assert_eq!(state.tuning().config.cache_ttl_secs, 900);

    for (name, value) in [
        (SettingName::MaxAttempts, json!(0)),
        (SettingName::AffinityMinHeadroom, json!(1.5)),
        (SettingName::CacheTtlSecs, json!(true)),
        (SettingName::ConsumersAllowLoopback, json!(true)),
        (SettingName::DaemonsAllowLoopback, json!(true)),
    ] {
        assert_eq!(
            root.set(name, value.clone()).await,
            Err(400),
            "{name} {value}"
        );
    }
    assert!(!state.tuning().config.consumers.allow_loopback);
    let bad_name = root
        .request(PUT, &format!("{SETTINGS_PATH}/values/listen"))
        .json(&json!({ "value": 1 }));
    assert_eq!(root.settings_with(bad_name).await, Err(404));

    let view = root
        .settings(DELETE, &setting_path(SettingName::MaxAttempts))
        .await
        .unwrap();
    let tunable = view
        .values
        .iter()
        .find(|tunable| tunable.name == SettingName::MaxAttempts)
        .unwrap();
    assert_eq!(tunable.value, SettingValue::Whole(3));
    assert!(!tunable.overridden);
    assert_eq!(state.tuning().config.max_attempts, 3);
    assert_eq!(state.tuning().config.affinity_min_headroom, 0.25);
}

#[tokio::test]
async fn loopback_can_be_allowed_when_not_proxy_only() {
    let state = state_exact(Config {
        trusted_proxy: TrustedProxy {
            addrs: vec!["127.0.0.1".parse().unwrap()],
            only: false,
            ..TrustedProxy::default()
        },
        ..proxy_config()
    });
    let addr = serve(state.clone()).await;
    let root = As::proxied(addr, Some(ROOT));
    let local = As::from(addr, "127.0.0.9");
    assert_eq!(local.status(GET, OVERVIEW_PATH).await, 403);
    root.set(SettingName::ConsumersAllowLoopback, json!(true))
        .await
        .unwrap();
    assert_eq!(local.status(GET, OVERVIEW_PATH).await, 200);
    let session = local.session().await.unwrap();
    assert!(session.consumer && !session.admin);
    assert_eq!(local.status(GET, SETTINGS_PATH).await, 403);
    root.settings(DELETE, &setting_path(SettingName::ConsumersAllowLoopback))
        .await
        .unwrap();
    assert_eq!(local.status(GET, OVERVIEW_PATH).await, 403);
}

#[cfg(unix)]
const TAILNET: &[(&str, &str)] = &[
    ("127.0.0.11", ROOT),
    ("127.0.0.12", ALICE),
    ("127.0.0.13", CAROL),
    ("127.0.0.14", TAGGED),
];

#[cfg(unix)]
#[tokio::test]
async fn the_same_flows_work_by_whois() {
    let state = state_exact(Config {
        trusted_proxy: TrustedProxy::default(),
        tailscale_socket: fake_tailnet(TAILNET),
        ..proxy_config()
    });
    let addr = serve(state.clone()).await;
    let root = As::from(addr, "127.0.0.11");
    let alice = As::from(addr, "127.0.0.12");
    let carol = As::from(addr, "127.0.0.13");
    let tagged = As::from(addr, "127.0.0.14");
    let unknown = As::from(addr, "127.0.0.15");

    assert!(root.session().await.unwrap().admin);
    assert!(!alice.session().await.unwrap().admin);
    assert_eq!(alice.status(GET, SETTINGS_PATH).await, 403);
    for caller in [&tagged, &unknown] {
        assert_eq!(caller.session().await, Err(403));
        assert_eq!(caller.ask(None).await, Err(403));
    }

    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 403);
    assert_eq!(carol.relay().await, 403);
    assert!(!carol.session().await.unwrap().consumer);
    carol.ask(Some("hello")).await.unwrap();
    assert_eq!(
        carol
            .ask(None)
            .await
            .unwrap()
            .request
            .unwrap()
            .note
            .as_deref(),
        Some("hello")
    );
    assert_eq!(root.session().await.unwrap().pending_requests, Some(1));
    root.settings(POST, &decision_path(CAROL, true))
        .await
        .unwrap();
    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 200);

    assert_eq!(carol.report("carol-box").await, 403);
    root.settings(PUT, &daemon_login(CAROL)).await.unwrap();
    assert_eq!(carol.report("carol-box").await, 204);
    root.settings(DELETE, &daemon_login(CAROL)).await.unwrap();
    assert_eq!(carol.report("carol-box").await, 403);

    assert_eq!(tagged.report("tagged-box").await, 403);
    root.settings(
        PUT,
        &entry_path(ListGroup::Daemons, ListKind::Tags, "tag:mesh"),
    )
    .await
    .unwrap();
    assert_eq!(tagged.report("tagged-box").await, 204);
    assert_eq!(tagged.status(GET, OVERVIEW_PATH).await, 403);

    root.set(SettingName::MaxAttempts, json!(7)).await.unwrap();
    assert_eq!(state.tuning().config.max_attempts, 7);
    root.settings(DELETE, &consumer_login(CAROL)).await.unwrap();
    assert_eq!(carol.status(GET, OVERVIEW_PATH).await, 403);
    assert_eq!(
        root.settings(DELETE, &consumer_login(ALICE)).await,
        Err(409)
    );
}
