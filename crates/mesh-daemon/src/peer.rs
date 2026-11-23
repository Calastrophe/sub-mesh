//! The server identity check: the daemon decides which server it dials.

use std::net::SocketAddr;

use mesh_relay::tunnel::Dial;

use crate::state::AppState;

/// A tagged node has a shared identity marker instead of a user login.
pub fn owner_login(login: &str) -> Option<&str> {
    let login = login.trim();
    (!login.is_empty() && !login.eq_ignore_ascii_case("tagged-devices")).then_some(login)
}

/// Check that `server`, the address of the host behind `server_url`, is a
/// server this daemon trusts (see [`crate::config::PeerPolicy`]). The daemon sends
/// nothing to the server before this passes. The error says why the server
/// was refused.
pub async fn verify_server(state: &AppState, server: SocketAddr) -> Result<(), String> {
    let policy = &state.settings.peers;
    if policy.skip_whois || (policy.allow_loopback && server.ip().is_loopback()) {
        return Ok(());
    }
    let whois = state
        .local
        .whois(server)
        .await
        .map_err(|err| format!("cannot check the server's identity: {err}"))?;
    if whois.tags.iter().any(|tag| policy.tags.contains(tag)) {
        return Ok(());
    }
    let login = &whois.login_name;
    if !login.trim().is_empty() {
        let allowed = if policy.logins.is_empty() {
            state
                .refresh_owner()
                .await
                .is_some_and(|owner| owner.eq_ignore_ascii_case(login))
        } else {
            policy
                .logins
                .iter()
                .any(|peer| peer.eq_ignore_ascii_case(login))
        };
        if allowed {
            return Ok(());
        }
    }
    Err(format!(
        "server {server} runs as {:?}, which allowed_peers does not trust",
        whois.login_name
    ))
}

/// Check the server on `dial` before sending it anything. Over `https://`
/// the TLS certificate for `server_url`'s host has already identified it
/// (the host may be a reverse proxy, which tailnet whois could not
/// identify); over `http://` its tailnet identity is checked with
/// [`verify_server`].
pub async fn verify_dial(state: &AppState, dial: &Dial) -> Result<(), String> {
    if dial.is_tls() {
        return Ok(());
    }
    verify_server(state, dial.peer_addr()).await
}

#[cfg(test)]
mod tests;
