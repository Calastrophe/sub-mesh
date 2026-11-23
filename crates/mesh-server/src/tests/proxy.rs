//! Requests through a trusted reverse proxy. The tests serve on loopback:
//! the consumer tests trust `127.0.0.1` as the proxy; the daemon tests put a
//! simulated proxy in front of the server that connects from `127.0.0.2`.

use std::sync::atomic::Ordering;

use axum::Router;
use mesh_core::protocol::REPORT_PATH;

use super::{account, apply, relays, report, serve};
use crate::AppState;
use crate::config::{Config, PeerPolicy, TrustedProxy};
use crate::peer::PROXY_NODE;
#[cfg(unix)]
use crate::testing::fake_tailscaled;
use crate::testing::{FakeDaemon, PROXY_HEADER, Reply, generation_of, start_proxy, state_exact};

use super::{Arc, SocketAddr};

const ALICE: &str = "alice@example.com";

/// A strict server behind a proxy on loopback that admits only Alice, as a
/// consumer and as a daemon. Whois is unreachable, so any request that
/// needed it would be refused.
async fn proxied_state(router: Router) -> Arc<AppState> {
    let state = state_exact(Config {
        consumers: PeerPolicy {
            logins: vec![ALICE.to_owned()],
            ..PeerPolicy::default()
        },
        daemons: PeerPolicy {
            logins: vec![ALICE.to_owned()],
            ..PeerPolicy::default()
        },
        trusted_proxy: TrustedProxy {
            addrs: vec!["127.0.0.1".parse().unwrap()],
            ..TrustedProxy::default()
        },
        tailscale_socket: "/nonexistent/tailscaled.sock".into(),
        web_dir: None,
        ..Config::default()
    });
    FakeDaemon::connect(&state, "node", router).await;
    apply(&state, report("node", vec![account("a", 0.9)]), None).await;
    state
}

fn post(addr: SocketAddr, path: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new().post(format!("http://{addr}{path}"))
}

#[tokio::test]
async fn the_proxy_names_the_caller_and_the_name_goes_no_further() {
    let stub = relays(vec![("a", Reply::status(200).body("served"))]);
    let addr = serve(proxied_state(stub.router()).await).await;
    let response = post(addr, "/codex/responses")
        .header("x-webauth-user", ALICE)
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].headers.contains_key("x-webauth-user"));

    let overview: mesh_core::api::Overview = reqwest::Client::new()
        .get(format!("http://{addr}/api/v1/overview"))
        .header("x-webauth-user", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview.me.login.as_deref(), Some(ALICE));
    assert_eq!(overview.me.node, PROXY_NODE);
}

#[tokio::test]
async fn the_proxy_must_name_an_allowed_user_plainly() {
    let stub = relays(vec![]);
    let addr = serve(proxied_state(stub.router()).await).await;
    let send = |request: reqwest::RequestBuilder| async move {
        request.body("payload").send().await.unwrap().status()
    };
    assert_eq!(send(post(addr, "/codex/responses")).await, 403);
    for login in ["bob@example.com", "", "tagged-devices"] {
        let request = post(addr, "/codex/responses").header("x-webauth-user", login);
        assert_eq!(send(request).await, 403, "{login}");
    }
    let twice = post(addr, "/codex/responses")
        .header("x-webauth-user", ALICE)
        .header("x-webauth-user", ALICE);
    assert_eq!(send(twice).await, 403);
    let cross_site = post(addr, "/claude/v1/messages")
        .header("x-webauth-user", ALICE)
        .header("sec-fetch-site", "cross-site");
    assert_eq!(send(cross_site).await, 403);
    assert!(stub.seen().is_empty());
}

/// Where the simulated proxy of the daemon tests connects from. It is a
/// loopback address other than `127.0.0.1`, so direct callers on
/// `127.0.0.1` still go through whois.
const PROXY_FROM: &str = "127.0.0.2";
const BOB: &str = "bob@example.com";

/// A server that trusts the proxy at [`PROXY_FROM`] and admits Alice and
/// Bob as daemons and Alice as a consumer. Whois answers `whois_login`
/// from a fake tailscaled, or is unreachable.
fn daemon_state(whois: Option<std::path::PathBuf>, only: bool) -> Arc<AppState> {
    state_exact(Config {
        consumers: PeerPolicy {
            logins: vec![ALICE.to_owned()],
            ..PeerPolicy::default()
        },
        daemons: PeerPolicy {
            logins: vec![ALICE.to_owned(), BOB.to_owned()],
            ..PeerPolicy::default()
        },
        trusted_proxy: TrustedProxy {
            addrs: vec![PROXY_FROM.parse().unwrap()],
            only,
            ..TrustedProxy::default()
        },
        tailscale_socket: whois.unwrap_or_else(|| "/nonexistent/tailscaled.sock".into()),
        web_dir: None,
        ..Config::default()
    })
}

fn tunnel_login(state: &AppState, node: &str) -> Option<String> {
    state.with_registry(|registry| {
        registry
            .tunnel(node)
            .and_then(|tunnel| tunnel.login.clone())
    })
}

async fn dial_refusal(addr: SocketAddr, node: &str) -> u16 {
    let url = url::Url::parse(&format!("http://{addr}")).unwrap();
    match mesh_relay::tunnel::dial(&url, node).await {
        Err(mesh_relay::Error::Rejected { status, .. }) => status.as_u16(),
        Err(error) => panic!("tunnel failed without a refusal: {error}"),
        Ok(_) => panic!("tunnel of {node} was accepted"),
    }
}

/// A consumer request through the proxy reaches a daemon whose tunnel came
/// through the same proxy, and the daemon is Alice's as whois would make it.
#[cfg(unix)]
#[tokio::test]
async fn a_daemon_tunnels_and_reports_through_the_proxy() {
    let (socket, whois_calls) = fake_tailscaled(ALICE);
    let state = daemon_state(Some(socket.clone()), false);
    let server = serve(state.clone()).await;
    let proxy = start_proxy(server, PROXY_FROM.parse().unwrap(), &[ALICE]).await;

    let stub = relays(vec![("a", Reply::status(200).body("served"))]);
    FakeDaemon::dial(&state, proxy, "laptop-1", stub.router()).await;
    assert_eq!(tunnel_login(&state, "laptop-1").as_deref(), Some(ALICE));
    let sent = post(proxy, REPORT_PATH)
        .header(PROXY_HEADER, BOB)
        .json(&report("laptop-1", vec![account("a", 0.9)]))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), 204);
    let owner = state.with_registry(|registry| registry.daemon_owner("laptop-1"));
    assert_eq!(owner.as_deref(), Some(ALICE));

    let response = post(proxy, "/codex/responses")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "served");
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].headers.contains_key(PROXY_HEADER));
    assert_eq!(whois_calls.load(Ordering::SeqCst), 0);

    // The direct tailnet path still works beside the proxy.
    FakeDaemon::dial(&state, server, "laptop-2", Router::new()).await;
    assert_eq!(tunnel_login(&state, "laptop-2").as_deref(), Some(ALICE));
    assert!(whois_calls.load(Ordering::SeqCst) >= 1);
    let _ = std::fs::remove_file(socket);
}

#[tokio::test]
async fn a_proxied_daemon_needs_one_allowed_login() {
    let state = daemon_state(None, false);
    let server = serve(state.clone()).await;
    let from = PROXY_FROM.parse().unwrap();
    for logins in [
        &[][..],
        &["carol@example.com"][..],
        &[ALICE, ALICE][..],
        &[ALICE, BOB][..],
        &[""][..],
        &["tagged-devices"][..],
    ] {
        let proxy = start_proxy(server, from, logins).await;
        assert_eq!(dial_refusal(proxy, "box").await, 403, "{logins:?}");
        let sent = post(proxy, REPORT_PATH)
            .json(&report("box", vec![account("b", 0.5)]))
            .send()
            .await
            .unwrap();
        assert_eq!(sent.status(), 403, "{logins:?}");
    }
    assert_eq!(generation_of(&state, "box"), None);
}

/// The node name a daemon sends is a label under its login, never an
/// identity: Bob cannot take over the node of Alice's daemon.
#[tokio::test]
async fn a_node_name_through_the_proxy_stays_with_its_login() {
    let state = daemon_state(None, true);
    let server = serve(state.clone()).await;
    let from = PROXY_FROM.parse().unwrap();
    let as_alice = start_proxy(server, from, &[ALICE]).await;
    let as_bob = start_proxy(server, from, &[BOB]).await;
    FakeDaemon::dial(&state, as_alice, "laptop", Router::new()).await;
    assert_eq!(dial_refusal(as_bob, "laptop").await, 409);
    let claim = post(as_bob, REPORT_PATH)
        .json(&report("laptop", vec![account("b", 0.5)]))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status(), 409);
    assert_eq!(tunnel_login(&state, "laptop").as_deref(), Some(ALICE));
    FakeDaemon::dial(&state, as_bob, "bob-box", Router::new()).await;
    assert_eq!(tunnel_login(&state, "bob-box").as_deref(), Some(BOB));
    assert_eq!(dial_refusal(as_bob, &"n".repeat(200)).await, 400);

    // With `only`, a direct caller is refused without asking whois.
    assert_eq!(dial_refusal(server, "direct").await, 403);
}
