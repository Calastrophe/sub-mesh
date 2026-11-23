use std::net::SocketAddr;

use crate::config::{Config, PeerPolicy};
use crate::testing::{TempDir, serve, state, state_exact, state_with};

async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let response = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

#[tokio::test]
async fn serves_the_dashboard_and_falls_back_to_it_for_app_routes() {
    let site = TempDir::new();
    site.write("index.html", "<h1>mesh dashboard</h1>");
    site.write("app.js", "console.log('app')");
    let state = state_with(Config {
        web_dir: Some(site.path().to_owned()),
        ..Config::default()
    });
    let addr = serve(state).await;

    let (status, body) = get(addr, "/").await;
    assert_eq!((status, body.as_str()), (200, "<h1>mesh dashboard</h1>"));
    assert_eq!(
        get(addr, "/app.js").await,
        (200, "console.log('app')".to_owned())
    );
    assert_eq!(
        get(addr, "/subscriptions/claude-1").await.1,
        "<h1>mesh dashboard</h1>"
    );

    assert_eq!(get(addr, "/healthz").await, (200, "ok".to_owned()));
    for path in [
        "/api/v1/nothing",
        "/api",
        "/claude",
        "/codex",
        "/mesh/v1/status",
        "/healthz/typo",
    ] {
        let (status, body) = get(addr, path).await;
        assert_eq!(status, 404, "{path}");
        assert!(!body.contains("dashboard"), "{path}");
    }
    let (status, body) = get(addr, "/api/v1/overview").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"subscriptions\""), "{body}");
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/codex/responses"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
}

#[tokio::test]
async fn says_so_when_no_dashboard_is_installed() {
    let addr = serve(state()).await;
    let (status, body) = get(addr, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("not installed"), "{body}");
    assert_eq!(get(addr, "/subscriptions").await.0, 404);
    assert_eq!(get(addr, "/healthz").await, (200, "ok".to_owned()));
    assert_eq!(get(addr, "/mesh/v1/status").await.0, 404);
}

#[tokio::test]
async fn the_management_api_follows_the_consumers_policy() {
    let strict = |allow_loopback| {
        state_exact(Config {
            consumers: PeerPolicy {
                allow_loopback,
                ..PeerPolicy::default()
            },
            tailscale_socket: "/nonexistent/tailscaled.sock".into(),
            web_dir: None,
            ..Config::default()
        })
    };
    let refused = serve(strict(false)).await;
    assert_eq!(get(refused, "/api/v1/overview").await.0, 403);
    assert_eq!(get(refused, "/api/v1/logs").await.0, 403);
    let allowed = serve(strict(true)).await;
    let (status, body) = get(allowed, "/api/v1/overview").await;
    assert_eq!(status, 200);
    let overview: mesh_core::api::Overview = serde_json::from_str(&body).unwrap();
    assert_eq!(overview.me.login, None);
    assert_eq!(overview.me.node, "loopback");
}
