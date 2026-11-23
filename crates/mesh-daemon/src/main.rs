//! `mesh-daemon`: holds this machine's subscription tokens and relays mesh
//! requests upstream.

mod api;
mod config;
mod peer;
mod poller;
mod relay;
mod reporter;
mod state;
mod tunnel;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, Settings};
use crate::state::{AppState, RuntimeSettings};

/// How long to wait for open connections after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(
    version,
    about = "Relay mesh requests through this machine's subscriptions"
)]
struct Cli {
    /// Config file (default: mesh/daemon.toml in the user config directory,
    /// ~/.config on Linux and %APPDATA% on Windows)
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let path = match cli.config {
        Some(path) => path,
        None => config::default_config_path()?,
    };
    let config = Config::load(&path)?;
    let local = mesh_relay::tailscale::LocalClient::new(config.tailscale_socket.clone());
    let own = match local.self_status().await {
        Ok(status) => Some(status),
        Err(err) => {
            tracing::warn!("cannot read tailnet status: {err}");
            None
        }
    };
    let skip_whois = config.insecure_skip_whois;
    let settings = config.resolve(own.as_ref(), &config::system_hostname())?;
    if skip_whois {
        tracing::warn!(
            "insecure_skip_whois is set: the daemon trusts ANY server at server_url without checking its tailnet identity, and that server may spend your subscriptions. Use this for development only."
        );
    }

    run(settings).await
}

async fn run(settings: Settings) -> Result<()> {
    let runtime = RuntimeSettings::load(&settings.runtime_path())
        .with_context(|| format!("cannot read {}", settings.runtime_path().display()))?;
    let upstream = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("cannot build the HTTP client")?;

    let admin_listen = settings.admin_listen;
    tracing::info!(
        node = %settings.node,
        server = %settings.server_url,
        "starting mesh-daemon"
    );
    let admin_listener = TcpListener::bind(admin_listen)
        .await
        .with_context(|| format!("cannot listen on {admin_listen}"))?;
    tracing::info!(%admin_listen, "listening");

    let admin_token = mesh_providers::read_or_create_secret(&settings.admin_token_path())
        .with_context(|| {
            format!(
                "cannot use the admin token {}",
                settings.admin_token_path().display()
            )
        })?;

    let state = AppState::new(settings, runtime, upstream);
    state.reload().await.context("cannot load accounts")?;
    tracing::info!(accounts = state.accounts().len(), "accounts loaded");

    let (signal_tx, signal_rx) = watch::channel(false);
    tokio::spawn(async move {
        mesh_relay::shutdown::requested().await;
        tracing::info!("shutting down");
        signal_tx.send_replace(true);
    });

    let admin = axum::serve(
        admin_listener,
        api::admin_router(state.clone(), admin_token),
    )
    .with_graceful_shutdown(signaled(signal_rx.clone()));

    let background = async {
        let reporting = async {
            tokio::select! {
                biased;
                () = signaled(signal_rx.clone()) => {}
                () = reporter::run(&state) => {}
            }
        };
        tokio::join!(reporting, tunnel::run(state.clone(), signal_rx.clone()));
        state.stop_background().await;
        Ok(())
    };
    let servers = async { tokio::try_join!(admin.into_future(), background) };
    tokio::select! {
        result = servers => {
            if let Err(err) = result {
                state.stop_background().await;
                return Err(err).context("server failed");
            }
        }
        () = async {
            signaled(signal_rx.clone()).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!("open connections did not close in time, exiting"),
    }
    Ok(())
}

async fn signaled(mut signal: watch::Receiver<bool>) {
    let _ = signal.wait_for(|signaled| *signaled).await;
}
