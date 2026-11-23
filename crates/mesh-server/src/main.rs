//! mesh-server: the always-on host. Consumers send provider requests here;
//! it picks an account and forwards the request, over the owning daemon's
//! tunnel, to that daemon. Daemons dial in; the server never dials out.

mod access;
mod api;
mod config;
mod db;
mod log;
mod peer;
mod registry;
mod relay;
mod report;
mod tunnel;
mod web;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::middleware;
use axum::routing::{any, get, post};
use chrono::{TimeDelta, Utc};
use clap::Parser;
use mesh_core::api::LOG_RETENTION_DAYS;
use mesh_core::{DaemonReport, protocol};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::db::Db;
use crate::log::Log;
use crate::peer::PeerChecker;
use crate::registry::Registry;

/// How long to keep logged requests.
const REQUEST_RETENTION: TimeDelta = TimeDelta::days(LOG_RETENTION_DAYS as i64);
/// How often to delete the requests older than [`REQUEST_RETENTION`].
const REQUEST_PRUNE_EVERY: u64 = 60;
/// How long in-flight requests get to finish at shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Parser)]
#[command(version, about = "Central mesh server")]
struct Cli {
    /// Config file. Default: /etc/mesh/server.toml if it exists, else
    /// mesh/server.toml in the user config directory (~/.config on Linux,
    /// %APPDATA% on Windows).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

/// What became of a daemon report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportOutcome {
    Applied,
    /// A live daemon of another login already uses the node name.
    NodeTaken,
}

/// Everything the handlers share.
pub struct AppState {
    pub config: Config,
    registry: Mutex<Registry>,
    reports: Mutex<()>,
    pub db: Db,
    pub peers: PeerChecker,
    request_log: Log,
    /// The connection of each daemon's current tunnel, with its generation.
    /// The registry decides which generation is current; see
    /// [`AppState::attach_tunnel`].
    clients: Mutex<HashMap<String, (u64, mesh_relay::tunnel::Client)>>,
    /// The merged model lists that the server has answered clients with.
    model_lists: relay::ModelLists,
}

impl AppState {
    /// Run `work` on the registry. Never hold the lock across an await.
    pub fn with_registry<T>(&self, work: impl FnOnce(&mut Registry) -> T) -> T {
        let mut registry = self.registry.lock().expect("registry mutex poisoned");
        work(&mut registry)
    }

    /// Store a report from the machine of `owner` and apply it to the
    /// registry. A report for a node that another login owns is refused.
    async fn apply_report(
        self: &Arc<Self>,
        mut report: DaemonReport,
        owner: Option<String>,
    ) -> anyhow::Result<ReportOutcome> {
        let state = self.clone();
        self.db
            .run(move |db| {
                let _guard = state.reports.lock().expect("report mutex poisoned");
                let now = Utc::now();
                let taken = state.with_registry(|registry| {
                    registry.node_taken(&report.node, owner.as_deref(), now)
                });
                if taken {
                    tracing::warn!(node = %report.node, ?owner, "refusing report for a node of another login");
                    return Ok(ReportOutcome::NodeTaken);
                }
                state.with_registry(|registry| registry.filter_report(&mut report, now));
                db.save_report(&report, owner.as_deref(), now)?;
                state.with_registry(|registry| registry.apply_report(report, owner, now));
                Ok(ReportOutcome::Applied)
            })
            .await
    }
}

fn router(state: Arc<AppState>) -> Router {
    let api = api::routes().layer(middleware::from_fn_with_state(
        state.clone(),
        access::consumer,
    ));
    let web = web::routes(state.config.web_dir.as_deref());
    Router::new()
        .route(protocol::REPORT_PATH, post(report::report))
        .route(protocol::TUNNEL_PATH, get(tunnel::tunnel))
        .route("/healthz", get(|| async { "ok" }))
        .route("/claude/{*path}", any(relay::claude))
        .route("/codex/{*path}", any(relay::codex))
        .merge(api)
        .merge(web)
        .with_state(state)
}

fn build_state(config: Config) -> anyhow::Result<Arc<AppState>> {
    let db = Db::open(&config.db_path)?;
    let stale =
        TimeDelta::from_std(config.daemon_stale()).context("daemon_stale_secs too large")?;
    let mut registry = Registry::new(
        stale,
        config.affinity_min_headroom,
        config.insecure_skip_whois,
    );
    let loaded = db.load().context("cannot load saved state")?;
    tracing::info!(
        daemons = loaded.daemons.len(),
        accounts = loaded.accounts.len(),
        "loaded saved state"
    );
    registry.load(loaded.daemons, loaded.accounts);
    registry.load_warmth(
        db.load_warmth(Utc::now())
            .context("cannot load cache warmth")?,
    );
    Ok(Arc::new(AppState {
        peers: PeerChecker::new(&config),
        config,
        registry: Mutex::new(registry),
        reports: Mutex::new(()),
        request_log: Log::start(db.clone())?,
        db,
        clients: Mutex::new(HashMap::new()),
        model_lists: relay::ModelLists::default(),
    }))
}

/// Housekeeping: expire cold caches and cooldowns, and delete request log
/// rows past [`REQUEST_RETENTION`] at start and then hourly.
async fn maintain(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut ticks: u64 = 0;
    loop {
        tick.tick().await;
        let now = Utc::now();
        state.with_registry(|registry| registry.prune(now));
        if let Err(error) = state.db.run(move |db| db.prune_warmth(now)).await {
            tracing::warn!(%error, "cannot prune cache warmth");
        }
        state.peers.prune();
        if ticks.is_multiple_of(REQUEST_PRUNE_EVERY) {
            let cutoff = now - REQUEST_RETENTION;
            match state.db.run(move |db| db.prune_requests(cutoff)).await {
                Ok(count) => tracing::debug!(count, "pruned old request log rows"),
                Err(error) => tracing::warn!(%error, "cannot prune request logs"),
            }
        }
        ticks += 1;
    }
}

async fn shutdown_signal() {
    mesh_relay::shutdown::requested().await;
    tracing::info!("shutting down");
}

async fn run(config: Config) -> anyhow::Result<()> {
    if config.insecure_skip_whois {
        tracing::warn!(
            "insecure_skip_whois is on: ANY peer that can reach {} may use your accounts. \
             Use this for development only.",
            config.listen
        );
    }
    let listen = config.listen;
    let state = tokio::task::spawn_blocking(move || build_state(config))
        .await
        .context("server initialization task failed")??;
    tokio::spawn(maintain(state.clone()));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    tracing::info!(%listen, "mesh-server ready");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
    });
    tokio::select! {
        result = &mut server => result.context("server task failed")??,
        () = shutdown_signal() => {
            let _ = stop_tx.send(());
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                Ok(result) => result.context("server task failed")??,
                Err(_) => {
                    server.abort();
                    tracing::warn!("closing with requests still in flight");
                }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    run(config).await
}

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
