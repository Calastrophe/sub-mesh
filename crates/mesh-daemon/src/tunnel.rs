//! The daemon's tunnel to the server.

#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use tokio::sync::watch;

use crate::state::AppState;
use crate::{peer, relay, signaled};

/// Delay before the first redial.
const INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Longest delay between redials.
const MAX_DELAY: Duration = Duration::from_secs(60);

/// A tunnel that stayed up for longer than this counts as healthy, and the
/// next redial starts again from [`INITIAL_DELAY`].
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// Exponential redial delays with jitter to spread out reconnections.
struct Backoff {
    base: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            base: INITIAL_DELAY,
        }
    }
}

impl Backoff {
    /// Reset after a stable tunnel. Jitter scales the base delay by 0.5–1.5.
    fn next_delay(&mut self, uptime: Duration, jitter: f64) -> Duration {
        if uptime > STABLE_AFTER {
            self.base = INITIAL_DELAY;
        }
        let delay = self.base.mul_f64(0.5 + jitter.clamp(0.0, 1.0));
        self.base = (self.base * 2).min(MAX_DELAY);
        delay.min(MAX_DELAY)
    }
}

/// How one connection attempt ended.
struct Outcome {
    /// How long the tunnel was up; zero if it never came up.
    uptime: Duration,
    /// Why the attempt ended.
    error: String,
}

impl Outcome {
    fn failed(error: String) -> Outcome {
        Outcome {
            uptime: Duration::ZERO,
            error,
        }
    }
}

/// Keep a tunnel to the server until `shutdown` is signaled.
pub async fn run(state: Arc<AppState>, shutdown: watch::Receiver<bool>) {
    let router = relay::router(state.clone());
    let mut backoff = Backoff::default();
    loop {
        let outcome = tokio::select! {
            biased;
            () = signaled(shutdown.clone()) => return,
            outcome = connect_once(&state, router.clone()) => outcome,
        };
        let delay = backoff.next_delay(outcome.uptime, rand::random());
        tracing::warn!(
            error = %outcome.error,
            retry_in = ?delay,
            "tunnel to {} is down",
            state.settings.server_url
        );
        state.record_tunnel_down(outcome.error);
        tokio::select! {
            biased;
            () = signaled(shutdown.clone()) => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Dial the server, check it, and serve `router` until the tunnel ends.
async fn connect_once(state: &Arc<AppState>, router: Router) -> Outcome {
    let settings = &state.settings;
    let dial = match mesh_relay::tunnel::Dial::connect(&settings.server).await {
        Ok(dial) => dial,
        Err(err) => return Outcome::failed(format!("cannot connect to the server: {err}")),
    };
    if let Err(reason) = peer::verify_dial(state, &dial).await {
        return Outcome::failed(reason);
    }
    let io = match dial.upgrade(&settings.node).await {
        Ok(io) => io,
        Err(err) => return Outcome::failed(describe_upgrade_failure(&err)),
    };
    tracing::info!(server = %settings.server_url, "tunnel to the server is up");
    state.record_tunnel_up();
    let started = Instant::now();
    let error = match mesh_relay::tunnel::serve(io, router).await {
        Ok(()) => "the server closed the tunnel".to_owned(),
        Err(err) => format!("the tunnel failed: {err}"),
    };
    Outcome {
        uptime: started.elapsed(),
        error,
    }
}

fn describe_upgrade_failure(err: &mesh_relay::Error) -> String {
    match err {
        mesh_relay::Error::Rejected { status, body, .. } => format!(
            "the server refused the tunnel with {status}: {}",
            String::from_utf8_lossy(body).trim()
        ),
        other => format!("the tunnel upgrade failed: {other}"),
    }
}
