//! Requests through a trusted reverse proxy. The tests serve on loopback,
//! so the proxy address is `127.0.0.1`.

use axum::Router;
use axum::http::Method;
use mesh_core::protocol::{REPORT_PATH, TUNNEL_PATH};

use super::{account, apply, relays, report, serve};
use crate::AppState;
use crate::config::{Config, PeerPolicy, TrustedProxy};
use crate::peer::PROXY_NODE;
use crate::testing::{FakeDaemon, Reply, state_exact};

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

#[tokio::test]
async fn daemon_routes_are_not_served_through_the_proxy() {
    let state = proxied_state(Router::new()).await;
    let addr = serve(state).await;
    let report = post(addr, REPORT_PATH)
        .header("x-webauth-user", ALICE)
        .json(&report("box", vec![account("b", 0.5)]))
        .send()
        .await
        .unwrap();
    assert_eq!(report.status(), 403);
    let tunnel = reqwest::Client::new()
        .request(Method::GET, format!("http://{addr}{TUNNEL_PATH}"))
        .header("x-webauth-user", ALICE)
        .header("x-mesh-node", "box")
        .send()
        .await
        .unwrap();
    assert_eq!(tunnel.status(), 403);
}
