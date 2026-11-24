use std::time::Duration;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::Method;
use axum::response::Response;
use axum::routing::any;
use chrono::{TimeDelta, Utc};
use futures_util::{SinkExt, StreamExt};
use mesh_core::protocol::{DENIED_HEADER, REPORT_PATH, TUNNEL_PATH};
use mesh_core::{AccountReport, Admission, DaemonReport, Provider, Window};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::config::{Config, PeerPolicy};
use crate::peer::Peer;
use crate::registry::{Caller, Scope, Warmth, WarmthRow};
use crate::relay::{self, Upgrade};
use crate::testing::{
    FakeDaemon, LOGIN_HEADER, Reply, Stub, account, apply, generation_of, report, serve, serve_app,
    serve_tunnels, state, state_exact, state_with, wait_for_detach,
};
use crate::{AppState, ReportOutcome};

use super::{Arc, SocketAddr};

#[tokio::test]
async fn rejects_live_account_claim_in_registry_and_database() {
    let state = state();
    let _a = FakeDaemon::connect(&state, "a", Router::new()).await;
    let _b = FakeDaemon::connect(&state, "b", Router::new()).await;
    apply(&state, report("a", vec![account("shared", 0.2)]), None).await;
    apply(
        &state,
        report("b", vec![account("shared", 0.9), account("other", 0.5)]),
        None,
    )
    .await;

    let candidates = state
        .with_registry(|r| r.candidates(Provider::Codex, &Caller::default(), None, Utc::now()));
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].id.as_str(), "other");
    assert_eq!(candidates[0].node, "b");
    assert_eq!(candidates[1].id.as_str(), "shared");
    assert_eq!(candidates[1].node, "a");
    let saved = state.db.run(crate::db::Db::load).await.unwrap();
    assert_eq!(saved.accounts.len(), 2);
    assert!(
        saved
            .accounts
            .iter()
            .any(|(node, account)| { node == "a" && account.id.as_str() == "shared" })
    );
}

#[tokio::test]
async fn a_node_name_cannot_be_taken_over_by_another_login() {
    let state = state();
    let node = || report("box", vec![account("a", 0.5)]);
    apply(&state, node(), Some("alice@example.com")).await;

    let outcome = state
        .apply_report(node(), Some("bob@example.com".to_owned()))
        .await
        .unwrap();
    assert_eq!(outcome, ReportOutcome::NodeTaken);
    let saved = state.db.run(crate::db::Db::load).await.unwrap();
    assert_eq!(saved.daemons[0].owner.as_deref(), Some("alice@example.com"));
    assert_eq!(
        owner_of_first_account(&state).as_deref(),
        Some("alice@example.com")
    );
    apply(&state, node(), Some("Alice@example.com")).await;
}

async fn post_report(addr: SocketAddr, report: &DaemonReport) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}{REPORT_PATH}"))
        .json(report)
        .send()
        .await
        .unwrap()
}

fn owner_of_first_account(state: &AppState) -> Option<String> {
    state
        .with_registry(|r| r.snapshot(Utc::now()))
        .accounts
        .remove(0)
        .owner
}

#[tokio::test]
async fn the_reported_owner_counts_only_when_whois_is_skipped() {
    let mut claim = report("box", vec![account("a", 0.5)]);
    claim.owner = Some("alice@example.com".to_owned());

    let skipping = state_with(Config::default());
    let addr = serve(skipping.clone()).await;
    assert_eq!(post_report(addr, &claim).await.status(), 204);
    assert_eq!(
        owner_of_first_account(&skipping).as_deref(),
        Some("alice@example.com")
    );

    let checked = state_exact(Config {
        daemons: PeerPolicy {
            allow_loopback: true,
            ..PeerPolicy::default()
        },
        web_dir: None,
        ..Config::default()
    });
    let addr = serve(checked.clone()).await;
    assert_eq!(post_report(addr, &claim).await.status(), 204);
    assert_eq!(owner_of_first_account(&checked), None);
}

#[tokio::test]
async fn a_node_of_another_login_gets_a_conflict_over_http() {
    let state = state_with(Config::default());
    let addr = serve(state.clone()).await;
    let mut first = report("box", vec![account("a", 0.5)]);
    first.owner = Some("alice@example.com".to_owned());
    assert_eq!(post_report(addr, &first).await.status(), 204);
    let mut second = first.clone();
    second.owner = Some("bob@example.com".to_owned());
    assert_eq!(post_report(addr, &second).await.status(), 409);
    second.owner = None;
    assert_eq!(post_report(addr, &second).await.status(), 409);
    assert_eq!(
        owner_of_first_account(&state).as_deref(),
        Some("alice@example.com")
    );
}

const RESPONSES: &str = "/responses";

fn relay_path(id: &str) -> String {
    format!("/relay/{id}{RESPONSES}")
}

/// A stub daemon that answers `POST /relay/{id}/responses` with each reply.
fn relays(replies: Vec<(&str, Reply)>) -> Arc<Stub> {
    let stub = Stub::new();
    for (id, reply) in replies {
        stub.on(Method::POST, &relay_path(id), reply);
    }
    stub
}

/// Check how often each account's relay route was called, and that nothing
/// else was.
fn expect_calls(stub: &Stub, expected: &[(&str, usize)]) {
    for (id, calls) in expected {
        assert_eq!(stub.calls(&relay_path(id)), *calls, "calls for {id}");
    }
    let total: usize = expected.iter().map(|(_, calls)| calls).sum();
    assert_eq!(stub.seen().len(), total, "{:?}", stub.seen());
}

async fn relay_state(router: Router, accounts: Vec<AccountReport>) -> Arc<AppState> {
    let state = state();
    FakeDaemon::connect(&state, "node", router).await;
    apply(&state, report("node", accounts), None).await;
    state
}

fn codex_request(addr: SocketAddr) -> reqwest::RequestBuilder {
    reqwest::Client::new().post(format!("http://{addr}/codex/responses"))
}

async fn send_codex(addr: SocketAddr, body: &str) -> reqwest::Response {
    codex_request(addr)
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
}

async fn post_codex(stub: &Arc<Stub>, accounts: Vec<AccountReport>) -> reqwest::Response {
    let state = relay_state(stub.router(), accounts).await;
    send_codex(serve(state).await, "payload").await
}

fn denied() -> Reply {
    Reply::status(429).header(DENIED_HEADER, "reserve")
}

#[tokio::test]
async fn a_request_goes_over_the_tunnel_with_its_path_query_and_body() {
    let stub = Stub::new();
    stub.on(
        Method::POST,
        "/relay/a/responses",
        Reply::status(200)
            .header("x-mesh-headroom", "0.25")
            .header("x-upstream", "yes")
            .body("served"),
    );
    let state = relay_state(stub.router(), vec![account("a", 0.7)]).await;
    let addr = serve(state.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/codex/responses?stream=true&q=a%2Fb"))
        .header("x-kept", "1")
        .header("x-mesh-internal", "secret")
        .header("x-webauth-user", "mallory@example.com")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-upstream"], "yes");
    assert!(!response.headers().contains_key("x-mesh-headroom"));
    assert_eq!(response.text().await.unwrap(), "served");

    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].query.as_deref(), Some("stream=true&q=a%2Fb"));
    assert_eq!(seen[0].body, "payload");
    assert_eq!(seen[0].headers["x-kept"], "1");
    assert!(!seen[0].headers.contains_key("x-mesh-internal"));
    assert!(!seen[0].headers.contains_key("x-webauth-user"));
    let view = state
        .with_registry(|r| r.view(&"a".parse().unwrap(), Utc::now()))
        .unwrap();
    assert_eq!(view.report.admission, Admission::Admit { headroom: 0.25 });
}

#[tokio::test]
async fn http_retries_mesh_denials_and_upstream_429s() {
    let stub = relays(vec![
        ("a", denied()),
        ("b", Reply::status(429).body("upstream rate limit")),
        ("c", Reply::status(200).body("served")),
    ]);
    let state = relay_state(
        stub.router(),
        vec![account("a", 0.9), account("b", 0.8), account("c", 0.7)],
    )
    .await;
    let addr = serve(state.clone()).await;
    let response = codex_request(addr)
        .header("session_id", "failover")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "served");
    expect_calls(&stub, &[("a", 1), ("b", 1), ("c", 1)]);
    let rows = stored_warmth(&state, 2).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|(_, warmth)| warmth.account.as_str() == "c")
    );
}

#[tokio::test]
async fn http_returns_the_last_upstream_429_when_no_account_serves() {
    let stub = relays(vec![
        ("a", denied()),
        ("b", Reply::status(429).body("upstream rate limit")),
    ]);
    let response = post_codex(&stub, vec![account("a", 0.9), account("b", 0.8)]).await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.text().await.unwrap(), "upstream rate limit");
    expect_calls(&stub, &[("a", 1), ("b", 1)]);
}

#[tokio::test]
async fn a_claude_throttle_is_passed_on_without_resting_the_account() {
    let path = "/relay/a/v1/messages";
    let stub = Stub::new();
    stub.on(
        Method::POST,
        path,
        Reply::status(429)
            .header("x-should-retry", "true")
            .body(r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#),
    );
    let claude = AccountReport {
        provider: Provider::Claude,
        ..account("a", 0.5)
    };
    let state = relay_state(stub.router(), vec![claude]).await;
    let addr = serve(state.clone()).await;
    let send = || {
        reqwest::Client::new()
            .post(format!("http://{addr}/claude/v1/messages"))
            .body("{}")
            .send()
    };
    assert_eq!(send().await.unwrap().status(), 429);
    let view = state
        .with_registry(|r| r.view(&"a".parse().unwrap(), Utc::now()))
        .unwrap();
    assert_eq!(view.cooldown_until, None);
    stub.on(Method::POST, path, Reply::status(200).body("served"));
    let response = send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "served");
    assert_eq!(stub.calls(path), 2);
}

#[tokio::test]
async fn a_rejected_claude_subscription_rests() {
    let path = "/relay/a/v1/messages";
    let stub = Stub::new();
    stub.on(
        Method::POST,
        path,
        Reply::status(429).header("anthropic-ratelimit-unified-status", "rejected"),
    );
    let claude = AccountReport {
        provider: Provider::Claude,
        ..account("a", 0.5)
    };
    let state = relay_state(stub.router(), vec![claude]).await;
    let addr = serve(state.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/claude/v1/messages"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    let view = state
        .with_registry(|r| r.view(&"a".parse().unwrap(), Utc::now()))
        .unwrap();
    assert!(view.cooldown_until.is_some());
}

#[tokio::test]
async fn failover_goes_to_the_next_best_score() {
    let stub = relays(vec![
        ("soon", denied()),
        ("mid", Reply::status(200).body("mid")),
        ("a-late", Reply::status(200).body("late")),
    ]);
    let response = post_codex(
        &stub,
        vec![
            weekly_account("a-late", 0.1, 150.0),
            weekly_account("mid", 0.1, 50.0),
            weekly_account("soon", 0.1, 2.0),
        ],
    )
    .await;
    assert_eq!(response.text().await.unwrap(), "mid");
    expect_calls(&stub, &[("soon", 1), ("mid", 1), ("a-late", 0)]);
}

/// A Codex account whose weekly window is `used` full and resets in `hours`.
fn weekly_account(id: &str, used: f64, hours: f64) -> AccountReport {
    let reserve = mesh_core::ReserveConfig::default();
    let window = Window {
        id: "secondary".to_owned(),
        duration_minutes: Some(10_080),
        used,
        resets_at: Some(Utc::now() + TimeDelta::seconds((hours * 3600.0) as i64)),
        limited: false,
    };
    let room = (1.0 - reserve.reserve_for(&window)) - used;
    AccountReport {
        windows: vec![window],
        ..account(id, room)
    }
}

/// Two daemons, each with one account, where `a` on `node-a` scores best.
async fn two_daemons(a: &Arc<Stub>, b: &Arc<Stub>) -> (Arc<AppState>, FakeDaemon, FakeDaemon) {
    let state = state();
    let daemon_a = FakeDaemon::connect(&state, "node-a", a.router()).await;
    let daemon_b = FakeDaemon::connect(&state, "node-b", b.router()).await;
    apply(&state, report("node-a", vec![account("a", 0.9)]), None).await;
    apply(&state, report("node-b", vec![account("b", 0.8)]), None).await;
    (state, daemon_a, daemon_b)
}

#[tokio::test]
async fn a_daemon_without_a_tunnel_is_skipped() {
    let a = relays(vec![("a", Reply::status(200).body("from a"))]);
    let b = relays(vec![("b", Reply::status(200).body("from b"))]);
    let (state, mut daemon_a, _daemon_b) = two_daemons(&a, &b).await;
    daemon_a.crash();
    daemon_a.closed().await;
    wait_for_detach(&state, "node-a", daemon_a.generation).await;

    let addr = serve(state.clone()).await;
    let response = send_codex(addr, "x").await;
    assert_eq!(response.text().await.unwrap(), "from b");
    expect_calls(&a, &[("a", 0)]);
    expect_calls(&b, &[("b", 1)]);
    let overview = state.with_registry(|r| r.snapshot(Utc::now()));
    let node_a = overview
        .daemons
        .iter()
        .find(|d| d.node == "node-a")
        .unwrap();
    assert!(!node_a.online);
    assert_eq!(node_a.connected_since, None);
}

#[tokio::test]
async fn a_candidate_whose_tunnel_vanished_rests_and_the_next_one_serves() {
    let a = relays(vec![("a", Reply::status(200).body("from a"))]);
    let b = relays(vec![("b", Reply::status(200).body("from b"))]);
    let (state, _daemon_a, _daemon_b) = two_daemons(&a, &b).await;
    let candidates = state
        .with_registry(|r| r.candidates(Provider::Codex, &Caller::default(), None, Utc::now()));
    assert_eq!(candidates[0].id.as_str(), "a");
    state.clients.lock().unwrap().remove("node-a");

    let request = Request::post("/codex/responses")
        .body(axum::body::Body::from("x"))
        .unwrap();
    let response = relay_codex(&state, request).await;
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), 64)
        .await
        .unwrap();
    assert_eq!(&body[..], b"from b");
    expect_calls(&a, &[("a", 0)]);
    expect_calls(&b, &[("b", 1)]);
    let view = state
        .with_registry(|r| r.view(&"a".parse().unwrap(), Utc::now()))
        .unwrap();
    assert!(view.cooldown_until.is_some(), "the daemon should rest");
}

#[tokio::test]
async fn a_request_that_began_on_a_daemon_is_not_retried_when_its_tunnel_breaks() {
    let began = Arc::new(Notify::new());
    let a = Router::new().fallback({
        let began = began.clone();
        move || {
            let began = began.clone();
            async move {
                began.notify_one();
                std::future::pending::<Response>().await
            }
        }
    });
    let b = relays(vec![("b", Reply::status(200).body("from b"))]);
    let state = state();
    let daemon_a = FakeDaemon::connect(&state, "node-a", a).await;
    let _daemon_b = FakeDaemon::connect(&state, "node-b", b.router()).await;
    apply(&state, report("node-a", vec![account("a", 0.9)]), None).await;
    apply(&state, report("node-b", vec![account("b", 0.8)]), None).await;
    let addr = serve(state.clone()).await;

    let request = tokio::spawn(async move { send_codex(addr, "x").await });
    began.notified().await;
    daemon_a.crash();
    let response = request.await.unwrap();
    assert_eq!(response.status(), 502);
    expect_calls(&b, &[("b", 0)]);
}

/// An account that scores far below an idle one: its weekly window has only
/// 0.03 room left, which is still above the affinity threshold.
fn nearly_spent(id: &str) -> AccountReport {
    let mut spent = weekly_account(id, 0.67, 100.0);
    spent.admission = Admission::Admit { headroom: 0.03 };
    spent
}

async fn session_relay() -> (Arc<AppState>, Arc<Stub>, SocketAddr) {
    let stub = relays(vec![
        ("a", Reply::status(200).body("from a")),
        ("b", Reply::status(200).body("from b")),
    ]);
    let state = relay_state(stub.router(), vec![account("a", 0.7), account("b", 0.7)]).await;
    let addr = serve(state.clone()).await;
    (state, stub, addr)
}

async fn send_session(addr: SocketAddr, session: Option<&str>, body: &str) -> String {
    let mut request = codex_request(addr)
        .header("content-type", "application/json")
        .body(body.to_owned());
    if let Some(session) = session {
        request = request.header("session_id", session);
    }
    request.send().await.unwrap().text().await.unwrap()
}

#[tokio::test]
async fn a_session_header_or_prompt_cache_key_keeps_the_session_on_its_account() {
    for (session, body) in [
        (Some("s-1"), "{}"),
        (
            None,
            r#"{"model":"gpt","prompt_cache_key":"conv-1","input":[]}"#,
        ),
    ] {
        let (state, stub, addr) = session_relay().await;
        assert_eq!(send_session(addr, session, body).await, "from a");
        apply(
            &state,
            report("node", vec![nearly_spent("a"), account("b", 0.7)]),
            None,
        )
        .await;
        assert_eq!(send_session(addr, session, body).await, "from a");
        assert_eq!(send_session(addr, None, "{}").await, "from b");
        expect_calls(&stub, &[("a", 2), ("b", 1)]);
    }
}

const ZSTD_EMPTY_OBJECT: &[u8] = b"\x28\xb5\x2f\xfd\x20\x02\x11\x00\x00{}";

#[tokio::test]
async fn codex_zstd_requests_keep_their_encoding_bytes_and_session() {
    let (state, stub, addr) = session_relay().await;
    assert_eq!(
        send_session(addr, None, r#"{"prompt_cache_key":"conv"}"#).await,
        "from a"
    );
    apply(
        &state,
        report("node", vec![nearly_spent("a"), account("b", 0.7)]),
        None,
    )
    .await;
    let response = codex_request(addr)
        .header("session-id", "conv")
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(ZSTD_EMPTY_OBJECT)
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "from a");
    let seen = stub.seen();
    assert_eq!(seen[1].body.as_ref(), ZSTD_EMPTY_OBJECT);
    assert_eq!(seen[1].headers["content-encoding"], "zstd");
}

#[tokio::test]
async fn a_session_leaves_an_account_that_ran_out_of_headroom() {
    let (state, stub, addr) = session_relay().await;
    assert_eq!(send_session(addr, Some("s-1"), "x").await, "from a");
    let mut spent = nearly_spent("a");
    spent.admission = Admission::Admit { headroom: 0.01 };
    apply(&state, report("node", vec![spent, account("b", 0.7)]), None).await;
    assert_eq!(send_session(addr, Some("s-1"), "x").await, "from b");
    expect_calls(&stub, &[("a", 1), ("b", 1)]);
}

async fn stored_warmth(state: &AppState, rows: usize) -> Vec<WarmthRow> {
    let mut stored = Vec::new();
    for _ in 0..50 {
        stored = state.db.run(|db| db.load_warmth(Utc::now())).await.unwrap();
        if stored.len() == rows {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stored
}

fn warmth_of<'a>(stored: &'a [WarmthRow], scope: Scope, name: &str) -> &'a Warmth {
    stored
        .iter()
        .find(|(key, _)| key.scope == scope && key.name == name)
        .map(|(_, warmth)| warmth)
        .unwrap_or_else(|| panic!("no warmth for {name}"))
}

async fn send_as(addr: SocketAddr, login: &str, session: &str) -> String {
    codex_request(addr)
        .header(LOGIN_HEADER, login)
        .header("session_id", session)
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_second_session_of_the_same_user_lands_on_the_user_warm_account() {
    let stub = relays(vec![
        ("a", Reply::status(200).body("from a")),
        ("b", Reply::status(200).body("from b")),
    ]);
    let state = relay_state(
        stub.router(),
        vec![
            weekly_account("a", 0.1, 50.0),
            weekly_account("b", 0.1, 100.0),
        ],
    )
    .await;
    let addr = serve_with_identity(state.clone()).await;
    assert_eq!(send_as(addr, "alice@example.com", "s1").await, "from a");

    let swapped = vec![
        weekly_account("a", 0.1, 100.0),
        weekly_account("b", 0.1, 60.0),
    ];
    apply(&state, report("node", swapped), None).await;
    assert_eq!(send_as(addr, "bob@example.com", "s2").await, "from b");
    assert_eq!(send_as(addr, "ALICE@example.com", "s3").await, "from a");

    let stored = stored_warmth(&state, 5).await;
    assert_eq!(stored.len(), 5);
    let ttl = TimeDelta::seconds(300);
    for (scope, name, account) in [
        (Scope::Session, "s1", "a"),
        (Scope::Session, "s2", "b"),
        (Scope::Session, "s3", "a"),
        (Scope::User, "alice@example.com", "a"),
        (Scope::User, "bob@example.com", "b"),
    ] {
        let warmth = warmth_of(&stored, scope, name);
        assert_eq!(warmth.account.as_str(), account, "{name}");
        assert_eq!(warmth.ttl, ttl, "{name}");
    }
}

#[tokio::test]
async fn a_request_that_asks_for_longer_retention_keeps_its_caches_warm_longer() {
    let (state, _stub, addr) = session_relay().await;
    let body = r#"{"prompt_cache_key":"conv","prompt_cache_retention":"24h"}"#;
    assert_eq!(send_session(addr, None, body).await, "from a");
    let stored = stored_warmth(&state, 2).await;
    assert_eq!(stored.len(), 2);
    for (scope, name) in [(Scope::Session, "conv"), (Scope::User, "127.0.0.1")] {
        assert_eq!(warmth_of(&stored, scope, name).ttl, TimeDelta::hours(24));
    }
}

fn in_flight(state: &AppState, id: &str) -> u32 {
    state
        .with_registry(|r| r.view(&id.parse().unwrap(), Utc::now()))
        .unwrap()
        .in_flight
}

async fn wait_for_in_flight(state: &AppState, id: &str, expected: u32) {
    for _ in 0..100 {
        if in_flight(state, id) == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "in_flight did not become {expected}: {}",
        in_flight(state, id)
    );
}

#[tokio::test]
async fn a_request_counts_as_in_flight_until_its_response_ends() {
    let stub = relays(vec![(
        "a",
        Reply::status(200)
            .delay(Duration::from_millis(500))
            .body("served"),
    )]);
    let state = relay_state(stub.router(), vec![account("a", 0.7)]).await;
    let addr = serve(state.clone()).await;
    let request = tokio::spawn(async move { send_codex(addr, "x").await.text().await.unwrap() });
    wait_for_in_flight(&state, "a", 1).await;
    assert_eq!(request.await.unwrap(), "served");
    wait_for_in_flight(&state, "a", 0).await;
}

/// Relay a consumer request that is not a WebSocket, without a listener.
async fn relay_codex(state: &Arc<AppState>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, state).await;
    relay::relay_as(
        state,
        Provider::Codex,
        Peer::anonymous("test"),
        crate::peer::Grant::Open,
        upgrade,
        Request::from_parts(parts, body),
    )
    .await
}

/// A daemon router that accepts a WebSocket on any relay route and echoes.
fn websocket_daemon() -> Router {
    async fn echo(upgrade: WebSocketUpgrade) -> Response {
        upgrade.on_upgrade(|mut socket| async move {
            while let Some(Ok(message)) = socket.recv().await {
                if socket.send(message).await.is_err() {
                    break;
                }
            }
        })
    }
    Router::new().route("/relay/{id}/{*path}", any(echo))
}

#[tokio::test]
async fn a_websocket_uses_the_account_warmed_by_compressed_http_before_the_first_frame() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let stub = relays(vec![
        ("a", Reply::status(200).body("from a")),
        ("b", Reply::status(200).body("from b")),
    ]);
    let daemon = websocket_daemon().route(
        "/relay/{id}/{*path}",
        axum::routing::post_service(stub.router()),
    );
    let state = relay_state(daemon, vec![account("a", 0.9), account("b", 0.1)]).await;
    let addr = serve(state.clone()).await;
    let response = codex_request(addr)
        .header("session-id", "compressed-session")
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(ZSTD_EMPTY_OBJECT)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "from a");
    let seen = stub.seen();
    assert_eq!(&seen[0].body[..], ZSTD_EMPTY_OBJECT);
    assert_eq!(seen[0].headers["content-encoding"], "zstd");
    apply(
        &state,
        report("node", vec![nearly_spent("a"), account("b", 0.9)]),
        None,
    )
    .await;

    assert_eq!(send_session(addr, None, "{}").await, "from b");
    let mut request = format!("ws://{addr}/codex/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("session-id", "compressed-session".parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket.send(Message::Text("{}".into())).await.unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::Text("{}".into())
    );
    assert_eq!(in_flight(&state, "a"), 1);
    socket.close(None).await.unwrap();
}

#[tokio::test]
async fn a_websocket_counts_as_in_flight_until_it_closes() {
    let state = relay_state(websocket_daemon(), vec![account("a", 0.7)]).await;
    let addr = serve(state.clone()).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/codex/responses"))
        .await
        .unwrap();
    socket.send(Message::Text("ping".into())).await.unwrap();
    let echoed = socket.next().await.unwrap().unwrap();
    assert_eq!(echoed, Message::Text("ping".into()));
    assert_eq!(in_flight(&state, "a"), 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(in_flight(&state, "a"), 1, "still open, so still counted");

    socket.close(None).await.unwrap();
    while socket.next().await.is_some() {}
    wait_for_in_flight(&state, "a", 0).await;
}

#[tokio::test]
async fn cancelling_a_relay_attempt_releases_its_in_flight_count() {
    let stub = relays(vec![(
        "a",
        Reply::status(200).delay(Duration::from_secs(3)),
    )]);
    let state = relay_state(stub.router(), vec![account("a", 0.7)]).await;
    let request = Request::post("/codex/responses")
        .body(axum::body::Body::from("x"))
        .unwrap();
    let attempt = tokio::spawn({
        let state = state.clone();
        async move { relay_codex(&state, request).await }
    });
    wait_for_in_flight(&state, "a", 1).await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    assert_eq!(in_flight(&state, "a"), 0);
}

#[tokio::test]
async fn a_failed_attempt_does_not_stay_in_flight() {
    let stub = relays(vec![("a", denied())]);
    let state = relay_state(stub.router(), vec![account("a", 0.7)]).await;
    let addr = serve(state.clone()).await;
    let response = send_codex(addr, "x").await;
    assert_eq!(response.status(), 503);
    wait_for_in_flight(&state, "a", 0).await;
}

#[tokio::test]
async fn a_tunnel_replaces_the_older_one_of_the_same_node() {
    let old = relays(vec![("a", Reply::status(200).body("old"))]);
    let new = relays(vec![("a", Reply::status(200).body("new"))]);
    let state = state();
    let addr = serve_tunnels(state.clone()).await;
    let mut first = FakeDaemon::dial(&state, addr, "node", old.router()).await;
    let second = FakeDaemon::dial(&state, addr, "node", new.router()).await;
    assert_ne!(first.generation, second.generation);
    first.closed().await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(generation_of(&state, "node"), Some(second.generation));
    apply(&state, report("node", vec![account("a", 0.7)]), None).await;
    let addr = serve(state.clone()).await;
    let response = send_codex(addr, "x").await;
    assert_eq!(response.text().await.unwrap(), "new");
    expect_calls(&old, &[("a", 0)]);
}

#[tokio::test]
async fn an_old_tunnel_closing_does_not_unregister_the_new_one() {
    let state = state();
    let addr = serve_tunnels(state.clone()).await;
    let first = FakeDaemon::dial(&state, addr, "node", Router::new()).await;
    let second = FakeDaemon::dial(&state, addr, "node", Router::new()).await;
    assert!(!state.detach_tunnel("node", first.generation));
    assert_eq!(generation_of(&state, "node"), Some(second.generation));
    assert!(
        state
            .tunnel_client("node", second.generation)
            .is_some_and(|client| !client.is_closed())
    );
    assert!(state.tunnel_client("node", first.generation).is_none());
    assert!(state.detach_tunnel("node", second.generation));
    assert_eq!(generation_of(&state, "node"), None);
    assert!(state.tunnel_client("node", second.generation).is_none());
}

#[tokio::test]
async fn a_crashed_tunnel_is_unregistered_and_a_new_one_registers_again() {
    let state = state();
    let addr = serve_tunnels(state.clone()).await;
    let mut first = FakeDaemon::dial(&state, addr, "node", Router::new()).await;
    first.crash();
    first.closed().await;
    wait_for_detach(&state, "node", first.generation).await;
    assert_eq!(generation_of(&state, "node"), None);
    let again = FakeDaemon::dial(&state, addr, "node", Router::new()).await;
    assert_ne!(again.generation, first.generation);
}

/// Send a tunnel upgrade for `node` as `login`, and return the status.
async fn upgrade_status(addr: SocketAddr, node: &str, login: &str) -> u16 {
    reqwest::Client::new()
        .get(format!("http://{addr}{TUNNEL_PATH}"))
        .header("connection", "upgrade")
        .header("upgrade", mesh_core::protocol::TUNNEL_PROTOCOL)
        .header(mesh_core::protocol::NODE_HEADER, node)
        .header(LOGIN_HEADER, login)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn a_tunnel_for_a_node_of_another_login_gets_a_conflict() {
    let state = state();
    apply(
        &state,
        report("box", vec![account("a", 0.5)]),
        Some("alice@example.com"),
    )
    .await;
    let addr = serve_tunnels(state.clone()).await;
    assert_eq!(upgrade_status(addr, "box", "bob@example.com").await, 409);
    assert_eq!(generation_of(&state, "box"), None);
    assert_eq!(upgrade_status(addr, "box", "Alice@Example.com").await, 101);
    assert_eq!(upgrade_status(addr, "other", "bob@example.com").await, 101);
}

#[tokio::test]
async fn a_tunnel_must_be_a_valid_upgrade() {
    let state = state();
    let addr = serve(state.clone()).await;
    let response = reqwest::get(format!("http://{addr}{TUNNEL_PATH}"))
        .await
        .unwrap();
    assert_eq!(response.status(), 426);
}

#[tokio::test]
async fn the_real_route_registers_a_tunnel_and_shows_the_daemon_as_connected() {
    let state = state();
    let addr = serve(state.clone()).await;
    let _daemon = FakeDaemon::dial(&state, addr, "box", Router::new()).await;
    apply(&state, report("box", vec![account("a", 0.5)]), None).await;
    let snapshot = state.with_registry(|r| r.snapshot(Utc::now()));
    assert!(snapshot.daemons[0].online);
    assert!(snapshot.daemons[0].connected_since.is_some());
}

#[tokio::test]
async fn peers_outside_the_daemons_policy_cannot_open_a_tunnel() {
    let state = state_exact(Config {
        web_dir: None,
        ..Config::default()
    });
    let addr = serve(state.clone()).await;
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let Err(mesh_relay::Error::Rejected { status, .. }) =
        mesh_relay::tunnel::dial(&url, "box").await
    else {
        panic!("expected the tunnel to be refused");
    };
    assert_eq!(status, 403);
    assert_eq!(generation_of(&state, "box"), None);
}

#[tokio::test]
async fn a_report_from_another_login_than_the_tunnel_gets_a_conflict() {
    let state = state_with(Config::default());
    let attached = state.with_registry(|registry| {
        registry.attach_tunnel("box", Some("alice@example.com".to_owned()), Utc::now())
    });
    assert!(attached.is_some());
    let mut claim = report("box", vec![account("a", 0.5)]);
    claim.owner = Some("bob@example.com".to_owned());
    let outcome = state
        .apply_report(claim, Some("bob@example.com".to_owned()))
        .await;
    assert_eq!(outcome.unwrap(), ReportOutcome::NodeTaken);
}

/// Whois cannot be faked over loopback, so this router takes the caller's
/// login from a header and passes it to the relay as an identity value.
async fn relay_as_header(
    State(state): State<Arc<AppState>>,
    upgrade: Upgrade,
    request: Request,
) -> Response {
    let provider = if request.uri().path().starts_with("/claude") {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let login = request
        .headers()
        .get(LOGIN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let peer = Peer {
        node: "test-node".to_owned(),
        login,
    };
    relay::relay_as(
        &state,
        provider,
        peer,
        crate::peer::Grant::Open,
        upgrade,
        request,
    )
    .await
}

async fn serve_with_identity(state: Arc<AppState>) -> SocketAddr {
    let app = Router::new()
        .route("/claude/{*path}", any(relay_as_header))
        .route("/codex/{*path}", any(relay_as_header))
        .with_state(state);
    serve_app(app).await
}

#[tokio::test]
async fn requests_are_logged_with_the_callers_login_and_account() {
    let stub = relays(vec![("a", Reply::status(200).body("served"))]);
    let state = relay_state(stub.router(), vec![account("a", 0.9)]).await;
    let addr = serve_with_identity(state.clone()).await;
    for login in [
        "partner@example.com",
        "partner@example.com",
        "kid@example.com",
    ] {
        let response = codex_request(addr)
            .header(LOGIN_HEADER, login)
            .body("payload")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    expect_calls(&stub, &[("a", 3)]);

    let since = Utc::now() - TimeDelta::hours(1);
    let mut activity = Default::default();
    for _ in 0..50 {
        activity = state.db.run(move |db| db.activity(since)).await.unwrap();
        if activity.by_login.iter().map(|c| c.requests).sum::<u64>() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let count_of = |login: &str| {
        activity
            .by_login
            .iter()
            .find(|c| c.login == login)
            .map(|c| c.requests)
    };
    assert_eq!(count_of("partner@example.com"), Some(2));
    assert_eq!(count_of("kid@example.com"), Some(1));
    assert_eq!(activity.by_account.get("a"), Some(&3));
}

#[tokio::test]
async fn no_account_with_headroom_is_a_plain_503() {
    let state = state();
    let addr = serve_with_identity(state.clone()).await;
    let response = codex_request(addr)
        .header(LOGIN_HEADER, "someone@example.com")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "mesh_unavailable");
    assert_eq!(body["error"]["message"], "mesh: no account has headroom");
}

mod logs;
mod models;
mod proxy;
mod settings;
mod web;
mod websocket;
