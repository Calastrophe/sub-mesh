//! The route where daemons dial in and the registry of their tunnels.
//!
//! A daemon opens one tunnel to [`mesh_core::protocol::TUNNEL_PATH`] and keeps it. The
//! server sends every relay and control call over it, as HTTP/2 streams.
//! See [`mesh_core::protocol`] for the protocol.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use hyper::upgrade::OnUpgrade;
use mesh_relay::tunnel::accept;

use crate::AppState;
use crate::access::authorize;
use crate::peer::Role;

/// `GET TUNNEL_PATH`
pub async fn tunnel(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    match authorize(&state, addr, Role::Daemon).await {
        Ok(peer) => connect(state, peer.login, request),
        Err(response) => *response,
    }
}

/// Answer a tunnel upgrade from the machine of `login`.
///
/// `login` is the whois login of the peer, or `None` without whois. A
/// node name that another login holds is refused with `409`. Otherwise the
/// `101` goes back at once, and a task waits for the upgrade, registers the
/// tunnel, and unregisters it when it closes.
pub(crate) fn connect(state: Arc<AppState>, login: Option<String>, request: Request) -> Response {
    let accepted = match accept(request) {
        Ok(accepted) => accepted,
        Err(refusal) => return refusal.into_response(),
    };
    let insecure = state.config.insecure_skip_whois;
    let (login, taken) = state.with_registry(|registry| {
        let login = tunnel_login(login, insecure, registry.daemon_owner(&accepted.node));
        let taken = registry.node_taken(&accepted.node, login.as_deref(), Utc::now());
        (login, taken)
    });
    if taken {
        tracing::warn!(node = %accepted.node, ?login, "refusing tunnel for a node of another login");
        return (
            StatusCode::CONFLICT,
            "this node name belongs to a daemon of another login\n",
        )
            .into_response();
    }
    tokio::spawn(run(state, accepted.node, login, accepted.upgrade));
    accepted.response
}

/// The login a tunnel is attributed to.
///
/// This is the whois login when there is one. Without whois
/// (`insecure_skip_whois`, development only) a tunnel has no verified login,
/// so it takes over the owner the node already has. Otherwise a restarted
/// development daemon would be refused as "another login" until its earlier
/// reports went stale.
fn tunnel_login(
    whois: Option<String>,
    insecure: bool,
    known_owner: Option<String>,
) -> Option<String> {
    let whois = whois
        .map(|login| login.trim().to_owned())
        .filter(|login| !login.is_empty());
    match whois {
        Some(login) => Some(login),
        None if insecure => known_owner,
        None => None,
    }
}

/// Own one tunnel from its upgrade to its end.
async fn run(state: Arc<AppState>, node: String, login: Option<String>, upgrade: OnUpgrade) {
    let handshake = mesh_relay::tunnel::Client::from_upgrade(upgrade);
    let Ok(result) = tokio::time::timeout(Duration::from_secs(10), handshake).await else {
        tracing::warn!(%node, "tunnel handshake timed out");
        return;
    };
    let client = match result {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%node, %error, "tunnel handshake failed");
            return;
        }
    };
    let closed = client.closed();
    let attach = {
        let state = state.clone();
        let node = node.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = state.reports.lock().expect("report mutex poisoned");
            state.attach_tunnel(&node, client, login, Utc::now())
        })
    };
    let Ok(Some(generation)) = attach.await else {
        tracing::warn!(%node, "refusing tunnel for a node of another login");
        return;
    };
    tracing::info!(%node, generation, "daemon tunnel up");
    closed.await;
    if state.detach_tunnel(&node, generation) {
        tracing::info!(%node, generation, "daemon tunnel down");
    }
}

impl AppState {
    /// Make `client` the tunnel of `node` and return its generation, unless
    /// another login holds the node name. An older tunnel of the node is
    /// dropped: it closes once its streams in progress have finished.
    pub fn attach_tunnel(
        &self,
        node: &str,
        client: mesh_relay::tunnel::Client,
        login: Option<String>,
        now: DateTime<Utc>,
    ) -> Option<u64> {
        self.with_registry(|registry| {
            let generation = registry.attach_tunnel(node, login, now)?;
            let mut clients = self.clients.lock().expect("tunnel mutex poisoned");
            clients.insert(node.to_owned(), (generation, client));
            Some(generation)
        })
    }

    /// Forget the tunnel of `node` if it still has `generation`. A tunnel
    /// that a newer one replaced must not remove the newer one.
    pub fn detach_tunnel(&self, node: &str, generation: u64) -> bool {
        self.with_registry(|registry| {
            let current = registry.detach_tunnel(node, generation);
            if current {
                self.clients
                    .lock()
                    .expect("tunnel mutex poisoned")
                    .remove(node);
            }
            current
        })
    }

    /// The connection of `node`'s tunnel, when it still has `generation`
    /// and is open. The registry decides which tunnel is current; this only
    /// finds its connection.
    pub fn tunnel_client(&self, node: &str, generation: u64) -> Option<mesh_relay::tunnel::Client> {
        let clients = self.clients.lock().expect("tunnel mutex poisoned");
        clients
            .get(node)
            .filter(|(current, client)| *current == generation && !client.is_closed())
            .map(|(_, client)| client.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::tunnel_login;

    #[test]
    fn whois_login_wins() {
        assert_eq!(
            tunnel_login(Some(" alice@x ".into()), true, Some("bob@x".into())),
            Some("alice@x".to_owned())
        );
    }

    #[test]
    fn insecure_tunnel_inherits_the_known_owner() {
        assert_eq!(
            tunnel_login(None, true, Some("bob@x".into())),
            Some("bob@x".to_owned())
        );
        assert_eq!(tunnel_login(Some("  ".into()), true, None), None);
    }

    #[test]
    fn verified_mode_never_borrows_an_owner() {
        assert_eq!(tunnel_login(None, false, Some("bob@x".into())), None);
    }
}
