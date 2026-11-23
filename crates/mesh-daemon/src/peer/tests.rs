use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::routing::get;

use super::verify_server;
use crate::state::AppState;
use crate::tunnel::testing::daemon_state;

const OWNER: &str = "alice@example.com";

fn state(name: &str, extra: &str) -> Arc<AppState> {
    daemon_state(name, "http://server:7070", extra, "", false)
}

fn whois_reply(login: &str, tags: &str) -> String {
    format!(
        r#"{{"UserProfile":{{"LoginName":"{login}"}},"Node":{{"Name":"server.mesh.test.","Tags":{tags}}}}}"#
    )
}

fn tailscaled_app(whois: impl Fn() -> String + Clone + Send + Sync + 'static) -> Router {
    Router::new()
        .route("/localapi/v0/whois", get(move || async move { whois() }))
        .route(
            "/localapi/v0/status",
            get(|| async {
                r#"{"Self":{"UserID":42},"User":{"42":{"LoginName":"alice@example.com"}}}"#
            }),
        )
}

#[cfg(unix)]
fn serve_tailscaled(state: &AppState, whois: impl Fn() -> String + Clone + Send + Sync + 'static) {
    let socket = tokio::net::UnixListener::bind(&state.settings.tailscale_socket).unwrap();
    let app = tailscaled_app(whois);
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
}

#[cfg(windows)]
fn serve_tailscaled(state: &AppState, whois: impl Fn() -> String + Clone + Send + Sync + 'static) {
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = state.settings.tailscale_socket.clone();
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let app = tailscaled_app(whois);
    tokio::spawn(async move {
        loop {
            server.connect().await.unwrap();
            let next = ServerOptions::new().create(&name).unwrap();
            let connected = std::mem::replace(&mut server, next);
            let service = TowerToHyperService::new(app.clone());
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(connected), service)
                    .await;
            });
        }
    });
}

#[tokio::test]
async fn loopback_and_skip_whois_need_no_tailscaled() {
    let loopback = "127.0.0.1:7070".parse().unwrap();
    let remote = "100.64.0.9:7070".parse().unwrap();

    let strict = state("peer-strict", "");
    assert!(verify_server(&strict, loopback).await.is_err());

    let allow_loopback = state("peer-loopback", "[allowed_peers]\nallow_loopback = true");
    assert!(verify_server(&allow_loopback, loopback).await.is_ok());
    assert!(verify_server(&allow_loopback, remote).await.is_err());

    let insecure = state("peer-insecure", "insecure_skip_whois = true");
    assert!(verify_server(&insecure, remote).await.is_ok());
    for state in [strict, allow_loopback, insecure] {
        let _ = std::fs::remove_dir_all(&state.settings.data_dir);
    }
}

#[tokio::test]
async fn default_policy_trusts_a_server_with_the_machine_login() {
    let remote = "100.64.0.9:7070".parse().unwrap();
    let trusted = Arc::new(AtomicBool::new(true));
    let state = state("peer-same-login", "");
    state.set_owner(Some(OWNER.to_owned()));
    let flag = trusted.clone();
    serve_tailscaled(&state, move || {
        if flag.load(Ordering::Relaxed) {
            whois_reply("Alice@Example.com", "null")
        } else {
            whois_reply("bob@example.com", "null")
        }
    });

    assert!(verify_server(&state, remote).await.is_ok());
    trusted.store(false, Ordering::Relaxed);
    let err = verify_server(&state, remote).await.unwrap_err();
    assert!(err.contains("bob@example.com"), "{err}");
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn logins_and_tags_widen_trust() {
    let remote = "100.64.0.9:7070".parse().unwrap();
    let state = state(
        "peer-listed",
        "[allowed_peers]\nlogins = [\"carol@example.com\"]\ntags = [\"tag:mesh\"]",
    );
    state.set_owner(Some(OWNER.to_owned()));
    let reply = Arc::new(std::sync::Mutex::new(whois_reply(
        "carol@example.com",
        "null",
    )));
    let current = reply.clone();
    serve_tailscaled(&state, move || current.lock().unwrap().clone());

    assert!(verify_server(&state, remote).await.is_ok());
    *reply.lock().unwrap() = whois_reply(OWNER, "null");
    assert!(verify_server(&state, remote).await.is_err());
    *reply.lock().unwrap() = whois_reply("tagged-devices", r#"["tag:mesh"]"#);
    assert!(verify_server(&state, remote).await.is_ok());
    *reply.lock().unwrap() = whois_reply("tagged-devices", r#"["tag:other"]"#);
    assert!(verify_server(&state, remote).await.is_err());
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn unreachable_tailscaled_refuses_the_server() {
    let state = state("peer-no-tailscaled", "");
    let err = verify_server(&state, "100.64.0.9:7070".parse().unwrap())
        .await
        .unwrap_err();
    assert!(err.contains("cannot check the server's identity"), "{err}");
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn owner_is_learned_from_tailscaled_when_unknown() {
    let remote = "100.64.0.9:7070".parse().unwrap();
    let state = state("peer-owner-recovery", "");
    assert!(state.owner().is_none());
    serve_tailscaled(&state, || whois_reply(OWNER, "null"));
    assert!(verify_server(&state, remote).await.is_ok());
    assert_eq!(state.owner().as_deref(), Some(OWNER));
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn owner_is_read_again_after_it_expires() {
    let state = state("peer-owner-refresh", "");
    serve_tailscaled(&state, || whois_reply(OWNER, "null"));

    state.set_owner(Some("old@example.com".to_owned()));
    assert_eq!(
        state.refresh_owner().await.as_deref(),
        Some("old@example.com"),
        "a fresh owner is not read again"
    );

    state.expire_owner();
    assert_eq!(state.refresh_owner().await.as_deref(), Some(OWNER));
    assert_eq!(state.owner().as_deref(), Some(OWNER));
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn owner_stays_when_tailscaled_cannot_be_asked() {
    let state = state("peer-owner-stale", "");
    state.set_owner(Some(OWNER.to_owned()));
    state.expire_owner();
    assert_eq!(state.refresh_owner().await.as_deref(), Some(OWNER));
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}
