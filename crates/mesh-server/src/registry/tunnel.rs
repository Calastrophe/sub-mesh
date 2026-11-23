use chrono::{DateTime, Utc};

use super::Registry;

/// The registry's record of one daemon's tunnel. The connection itself is
/// kept by the application state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelInfo {
    /// Distinguishes this tunnel from older and newer ones of the same node.
    pub generation: u64,
    /// Tailnet login of the daemon's machine, when known.
    pub login: Option<String>,
    pub connected_since: DateTime<Utc>,
}

impl Registry {
    /// Record a new tunnel for `node` from the machine of `login` and return
    /// its generation. It replaces any older tunnel of the node and clears
    /// the daemon's cooldown, since a new connection shows that it is alive.
    ///
    /// Returns `None`, and records nothing, when another login holds the
    /// node name (see [`Registry::node_taken`]).
    pub fn attach_tunnel(
        &mut self,
        node: &str,
        login: Option<String>,
        now: DateTime<Utc>,
    ) -> Option<u64> {
        if self.node_taken(node, login.as_deref(), now) {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.tunnels.insert(
            node.to_owned(),
            TunnelInfo {
                generation,
                login,
                connected_since: now,
            },
        );
        if let Some(daemon) = self.daemons.get_mut(node) {
            daemon.cooldown_until = None;
        }
        Some(generation)
    }

    /// Forget the tunnel of `node` if it still has `generation`. A tunnel
    /// that a newer one has replaced leaves the newer one alone. Returns
    /// whether a tunnel was removed.
    pub fn detach_tunnel(&mut self, node: &str, generation: u64) -> bool {
        let current = self
            .tunnels
            .get(node)
            .is_some_and(|tunnel| tunnel.generation == generation);
        if current {
            self.tunnels.remove(node);
        }
        current
    }

    /// The current tunnel of `node`, if it is connected.
    pub fn tunnel(&self, node: &str) -> Option<&TunnelInfo> {
        self.tunnels.get(node)
    }
}
