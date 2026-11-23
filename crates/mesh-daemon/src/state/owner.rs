use std::time::{Duration, Instant};

use super::AppState;
use crate::peer;

/// How long a known owner is trusted before tailscaled is asked again, so a
/// change of the machine's tailnet login needs no restart.
const OWNER_TTL: Duration = Duration::from_secs(60);

/// The owner login and when it was read.
#[derive(Debug)]
pub struct Owner {
    login: Option<String>,
    read_at: Option<Instant>,
}

impl Owner {
    pub fn new(login: Option<String>) -> Self {
        Self {
            login,
            read_at: Some(Instant::now()),
        }
    }

    fn fresh(&self) -> bool {
        self.login.is_some() && self.read_at.is_some_and(|at| at.elapsed() < OWNER_TTL)
    }
}

impl AppState {
    /// This machine's tailnet login as last read from tailscaled, or `None`
    /// when it is not known.
    pub fn owner(&self) -> Option<String> {
        self.owner
            .read()
            .expect("owner lock poisoned")
            .login
            .clone()
    }

    /// Set the cached login in a test.
    #[cfg(test)]
    pub fn set_owner(&self, owner: Option<String>) {
        *self.owner.write().expect("owner lock poisoned") = Owner::new(owner);
    }

    /// Treat the cached login in a test as read long ago.
    #[cfg(test)]
    pub fn expire_owner(&self) {
        self.owner.write().expect("owner lock poisoned").read_at = None;
    }

    /// The owner login. A known login is reused for [`OWNER_TTL`]. After
    /// that, and while it is not known, ask tailscaled again, so a daemon
    /// that started before tailscaled recovers and a changed login is
    /// noticed. When tailscaled cannot be asked, the last login stays.
    pub async fn refresh_owner(&self) -> Option<String> {
        if self.owner.read().expect("owner lock poisoned").fresh() {
            return self.owner();
        }
        match self.local.self_status().await {
            Ok(status) => {
                let login = peer::owner_login(&status.login_name).map(str::to_owned);
                *self.owner.write().expect("owner lock poisoned") = Owner::new(login.clone());
                login
            }
            Err(err) => {
                tracing::debug!("cannot read tailnet status: {err}");
                self.owner()
            }
        }
    }

    /// Whether the asserted login may manage subscriptions.
    pub fn allows_control(&self, owner: Option<&str>, login: &str) -> bool {
        owner
            .into_iter()
            .chain(self.settings.owners.iter().map(String::as_str))
            .any(|owner| owner.eq_ignore_ascii_case(login))
            || (self.settings.peers.skip_whois
                && owner.is_none()
                && self.settings.owners.is_empty())
    }
}
