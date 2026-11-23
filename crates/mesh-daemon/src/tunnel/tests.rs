use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use http::{Method, StatusCode};
use tokio::sync::watch;

use super::testing::{TestServer, Tunnel, WAIT, daemon_state};
use super::{Backoff, INITIAL_DELAY, MAX_DELAY, STABLE_AFTER, run};

const SHORT: Duration = Duration::from_secs(1);

#[test]
fn backoff_doubles_up_to_the_cap() {
    let mut backoff = Backoff::default();
    let delays: Vec<u64> = (0..9)
        .map(|_| backoff.next_delay(Duration::ZERO, 0.5).as_secs())
        .collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(MAX_DELAY, Duration::from_secs(60));
}

#[test]
fn backoff_jitter_spans_half_to_one_and_a_half() {
    let low = Backoff::default().next_delay(Duration::ZERO, 0.0);
    let high = Backoff::default().next_delay(Duration::ZERO, 1.0);
    assert_eq!(low, INITIAL_DELAY.mul_f64(0.5));
    assert_eq!(high, INITIAL_DELAY.mul_f64(1.5));
    assert_eq!(
        Backoff::default().next_delay(Duration::ZERO, 7.0),
        high,
        "jitter above one is clamped"
    );
    assert_eq!(
        Backoff::default().next_delay(Duration::ZERO, -3.0),
        low,
        "jitter below zero is clamped"
    );
}

#[test]
fn backoff_never_exceeds_the_cap_even_with_jitter() {
    let mut backoff = Backoff::default();
    for _ in 0..20 {
        assert!(backoff.next_delay(Duration::ZERO, 1.0) <= MAX_DELAY);
    }
    assert_eq!(backoff.next_delay(Duration::ZERO, 1.0), MAX_DELAY);
}

#[test]
fn backoff_resets_only_after_a_stable_tunnel() {
    let mut backoff = Backoff::default();
    for _ in 0..4 {
        backoff.next_delay(Duration::ZERO, 0.5);
    }
    assert_eq!(
        backoff.next_delay(STABLE_AFTER, 0.5),
        Duration::from_secs(16)
    );
    assert_eq!(
        backoff.next_delay(STABLE_AFTER + Duration::from_millis(1), 0.5),
        INITIAL_DELAY,
        "a tunnel that stayed up past the threshold resets the delay"
    );
    assert_eq!(
        backoff.next_delay(Duration::ZERO, 0.5),
        Duration::from_secs(2)
    );
}

#[tokio::test]
async fn dials_with_its_node_and_serves_relay_and_control_over_the_tunnel() {
    let server = TestServer::start().await;
    let state = daemon_state("tunnel-up", &server.url, "", "", true);
    let tunnel = Tunnel::connect(&state, server).await;
    assert_eq!(tunnel.node, "test-node");
    tokio::time::timeout(SHORT, state.wait_changed())
        .await
        .expect("a report was requested when the tunnel came up");

    let status = state.admin_status(Utc::now());
    assert!(status.tunnel_connected_since.is_some());
    assert!(status.tunnel_error.is_none());

    let reply = tunnel
        .request(Method::POST, "/relay/missing/v1/messages")
        .send()
        .await;
    assert_eq!(reply.status(), StatusCode::NOT_FOUND);
    assert_eq!(reply.text().await, "unknown account\n");

    let reply = tunnel
        .request(Method::POST, "/control/v1/accounts/missing/pause")
        .send()
        .await;
    assert_eq!(reply.status(), StatusCode::FORBIDDEN);

    let reply = tunnel.request(Method::GET, "/elsewhere").send().await;
    assert_eq!(reply.status(), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn redials_after_the_tunnel_drops() {
    let mut server = TestServer::start().await;
    let state = daemon_state("tunnel-redial", &server.url, "", "", true);
    let (shutdown, signal) = watch::channel(false);
    let task = tokio::spawn(run(state.clone(), signal));

    let (_, first) = server.next_client().await;
    drop(first);
    tokio::time::timeout(WAIT, async {
        while state.admin_status(Utc::now()).tunnel_error.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon notices the tunnel closed");
    assert!(
        state
            .admin_status(Utc::now())
            .tunnel_connected_since
            .is_none()
    );

    let (node, second) = server.next_client().await;
    assert_eq!(node, "test-node");
    assert!(!second.is_closed());
    assert_eq!(server.upgrades.load(Ordering::SeqCst), 2);
    tokio::time::timeout(WAIT, async {
        while state
            .admin_status(Utc::now())
            .tunnel_connected_since
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon records the new tunnel");
    assert!(state.admin_status(Utc::now()).tunnel_error.is_none());

    shutdown.send_replace(true);
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn untrusted_server_gets_no_upgrade() {
    let server = TestServer::start().await;
    let state = daemon_state("tunnel-untrusted", &server.url, "", "", false);
    let (shutdown, signal) = watch::channel(false);
    let task = tokio::spawn(run(state.clone(), signal));

    tokio::time::timeout(WAIT, async {
        while state.admin_status(Utc::now()).tunnel_error.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon records why it did not connect");
    let status = state.admin_status(Utc::now());
    assert!(status.tunnel_connected_since.is_none());
    assert!(
        status
            .tunnel_error
            .unwrap()
            .contains("cannot check the server's identity")
    );
    assert_eq!(server.upgrades.load(Ordering::SeqCst), 0);

    shutdown.send_replace(true);
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn shutdown_closes_the_tunnel() {
    let server = TestServer::start().await;
    let state = daemon_state("tunnel-shutdown", &server.url, "", "", true);
    let mut tunnel = Tunnel::connect(&state, server).await;
    let closed = tunnel.client.closed();
    tunnel.shut_down().await;
    tokio::time::timeout(WAIT, closed)
        .await
        .expect("the server sees the tunnel close");
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn refused_upgrade_is_reported_and_retried() {
    use axum::Router;
    use axum::routing::get;
    use mesh_core::protocol::TUNNEL_PATH;
    use tokio::net::TcpListener;

    let app = Router::new().route(
        TUNNEL_PATH,
        get(|| async { (StatusCode::FORBIDDEN, "not on the daemons list\n") }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = daemon_state("tunnel-refused", &url, "", "", true);
    let (shutdown, signal) = watch::channel(false);
    let task = tokio::spawn(run(state.clone(), signal));

    tokio::time::timeout(WAIT, async {
        while state.admin_status(Utc::now()).tunnel_error.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon records the refusal");
    let error = state.admin_status(Utc::now()).tunnel_error.unwrap();
    assert!(
        error.contains("403") && error.contains("not on the daemons list"),
        "{error}"
    );

    shutdown.send_replace(true);
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}
