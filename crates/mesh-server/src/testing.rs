//! Helpers shared by the tests that run the whole router.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use chrono::{TimeDelta, Utc};
use mesh_core::{AccountReport, Admission, DaemonReport, Provider};
use tokio::net::TcpListener;

use crate::config::Config;
use crate::db::Db;
use crate::registry::Registry;
use crate::relay::ModelLists;
use crate::{AppState, ReportOutcome, router};

mod daemon;
mod proxy;

pub use daemon::{
    FakeDaemon, LOGIN_HEADER, Reply, Seen, Stub, generation_of, serve_tunnels, wait_for_detach,
};
pub use proxy::{PROXY_HEADER, start_proxy};
#[cfg(unix)]
pub use proxy::{fake_tailnet, fake_tailscaled};

/// A state with peer checks off and no dashboard.
pub fn state() -> Arc<AppState> {
    state_with(Config {
        web_dir: None,
        ..Config::default()
    })
}

/// A state on an in-memory database with peer checks off.
pub fn state_with(config: Config) -> Arc<AppState> {
    state_exact(Config {
        insecure_skip_whois: true,
        ..config
    })
}

/// A state on an in-memory database that follows `config` exactly.
pub fn state_exact(config: Config) -> Arc<AppState> {
    state_with_lists(config, ModelLists::default())
}

/// A state like [`state_exact`] that keeps its merged model lists in `lists`.
pub fn state_with_lists(config: Config, model_lists: ModelLists) -> Arc<AppState> {
    let db = Db::open_in_memory().unwrap();
    let registry = Registry::new(
        TimeDelta::minutes(1),
        config.affinity_min_headroom,
        config.insecure_skip_whois,
    );
    AppState::new(config, db, registry, model_lists).unwrap()
}

/// A Codex account that admits traffic with `headroom` and has no windows.
pub fn account(id: &str, headroom: f64) -> AccountReport {
    AccountReport {
        id: id.parse().unwrap(),
        provider: Provider::Codex,
        label: None,
        windows: vec![],
        admission: Admission::Admit { headroom },
        paused: false,
        reserve: mesh_core::ReserveConfig::default(),
        models: None,
        updated_at: Utc::now(),
    }
}

pub fn report(node: &str, accounts: Vec<AccountReport>) -> DaemonReport {
    DaemonReport {
        node: node.to_owned(),
        owner: None,
        accounts,
        sent_at: Utc::now(),
    }
}

/// Apply a report from a daemon of `owner`, and expect it to be accepted.
pub async fn apply(state: &Arc<AppState>, report: DaemonReport, owner: Option<&str>) {
    let outcome = state
        .apply_report(report, owner.map(str::to_owned))
        .await
        .unwrap();
    assert_eq!(outcome, ReportOutcome::Applied);
}

/// Serve `app` on a free loopback port.
pub async fn serve_app(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// Serve the real router.
pub async fn serve(state: Arc<AppState>) -> SocketAddr {
    serve_app(router(state)).await
}

/// A directory under the system temp directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-server-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    pub fn write(&self, name: &str, text: &str) {
        std::fs::write(self.0.join(name), text).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
