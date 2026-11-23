//! Who may talk to the server: tailnet identity checks.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mesh_relay::tailscale::{self, LocalClient, WhoIs};
use tokio::sync::OnceCell;

use crate::config::{Config, PeerPolicy};

/// How long a successful whois answer is reused.
const WHOIS_TTL: Duration = Duration::from_secs(60);
const CACHE_LIMIT: usize = 256;

/// Which set of routes a request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Consumer routes and the management API.
    Consumer,
    /// The daemon report route.
    Daemon,
}

/// Who is calling, as far as the server can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// The peer's node name, or its IP address or `loopback` without whois.
    pub node: String,
    /// The tailnet login. `None` without whois.
    pub login: Option<String>,
}

impl Peer {
    /// A peer with no tailnet identity, such as a loopback connection.
    pub fn anonymous(node: &str) -> Peer {
        Peer {
            node: node.to_owned(),
            login: None,
        }
    }

    /// Who shares this peer's prompt caches: the login, or the node name
    /// when there is no login.
    pub fn user(&self) -> &str {
        self.login.as_deref().unwrap_or(&self.node)
    }
}

/// Tailscale's login for every tagged node. It names no user, so a tagged
/// peer has no login: it owns nothing and manages nothing.
const TAGGED_LOGIN: &str = "tagged-devices";

impl From<&WhoIs> for Peer {
    fn from(who: &WhoIs) -> Peer {
        let login = who.login_name.trim();
        Peer {
            node: who.node_name.clone(),
            login: (!login.is_empty() && !login.eq_ignore_ascii_case(TAGGED_LOGIN))
                .then(|| login.to_owned()),
        }
    }
}

/// The outcome of a peer check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed.
    Allow(Peer),
    /// Refused, with the reason for the log.
    Deny(String),
}

fn loopback_allowed(addr: SocketAddr, policy: &PeerPolicy) -> bool {
    policy.allow_loopback && addr.ip().to_canonical().is_loopback()
}

/// Decide whether the peer at `addr` may pass `policy`.
///
/// `own_login` is the server machine's login. It stands in for an empty
/// `logins` list.
pub fn decide(
    who: Result<&WhoIs, &tailscale::Error>,
    addr: SocketAddr,
    policy: &PeerPolicy,
    own_login: Option<&str>,
) -> Decision {
    if loopback_allowed(addr, policy) {
        return Decision::Allow(Peer::anonymous("loopback"));
    }
    let who = match who {
        Ok(who) => who,
        Err(error) => return Decision::Deny(format!("whois failed for {addr}: {error}")),
    };
    let login = who.login_name.trim();
    let matches_login = |allowed: &str| !login.is_empty() && allowed.eq_ignore_ascii_case(login);
    let login_ok = if policy.logins.is_empty() {
        own_login.is_some_and(matches_login)
    } else {
        policy.logins.iter().any(|allowed| matches_login(allowed))
    };
    let tag_ok = who.tags.iter().any(|tag| policy.tags.contains(tag));
    if login_ok || tag_ok {
        Decision::Allow(Peer::from(who))
    } else {
        Decision::Deny(format!(
            "{} ({}) is not allowed",
            who.node_name, who.login_name
        ))
    }
}

/// Runs peer checks, with a short whois cache.
pub struct PeerChecker {
    client: LocalClient,
    skip_whois: bool,
    consumers: PeerPolicy,
    daemons: PeerPolicy,
    own_login: OnceCell<String>,
    cache: Mutex<HashMap<IpAddr, (Instant, WhoIs)>>,
}

impl PeerChecker {
    pub fn new(config: &Config) -> PeerChecker {
        PeerChecker {
            client: LocalClient::new(config.tailscale_socket.clone()),
            skip_whois: config.insecure_skip_whois,
            consumers: config.consumers.clone(),
            daemons: config.daemons.clone(),
            own_login: OnceCell::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Check the peer at `addr` for `role`.
    pub async fn check(&self, addr: SocketAddr, role: Role) -> Decision {
        let policy = match role {
            Role::Consumer => &self.consumers,
            Role::Daemon => &self.daemons,
        };
        if self.skip_whois {
            return Decision::Allow(Peer::anonymous(&addr.ip().to_canonical().to_string()));
        }
        if loopback_allowed(addr, policy) {
            return Decision::Allow(Peer::anonymous("loopback"));
        }
        let own_login = if policy.logins.is_empty() {
            self.own_login().await
        } else {
            None
        };
        let who = self.whois(addr).await;
        decide(who.as_ref(), addr, policy, own_login)
    }

    async fn own_login(&self) -> Option<&str> {
        let login = self
            .own_login
            .get_or_try_init(|| async {
                self.client
                    .self_status()
                    .await
                    .map(|status| status.login_name)
            })
            .await;
        match login {
            Ok(login) => Some(login.as_str()),
            Err(error) => {
                tracing::warn!(%error, "cannot read this machine's tailnet login");
                None
            }
        }
    }

    async fn whois(&self, addr: SocketAddr) -> Result<WhoIs, tailscale::Error> {
        let ip = addr.ip().to_canonical();
        if let Some(who) = self.cached(ip) {
            return Ok(who);
        }
        let who = self.client.whois(addr).await?;
        let mut cache = self.cache.lock().expect("whois cache mutex poisoned");
        if cache.len() >= CACHE_LIMIT {
            cache.retain(|_, (at, _)| at.elapsed() < WHOIS_TTL);
        }
        if cache.len() < CACHE_LIMIT || cache.contains_key(&ip) {
            cache.insert(ip, (Instant::now(), who.clone()));
        }
        Ok(who)
    }

    pub fn prune(&self) {
        self.cache
            .lock()
            .expect("whois cache mutex poisoned")
            .retain(|_, (at, _)| at.elapsed() < WHOIS_TTL);
    }

    fn cached(&self, ip: IpAddr) -> Option<WhoIs> {
        let cache = self.cache.lock().expect("whois cache mutex poisoned");
        cache
            .get(&ip)
            .filter(|(at, _)| at.elapsed() < WHOIS_TTL)
            .map(|(_, who)| who.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(login: &str, tags: &[&str]) -> WhoIs {
        WhoIs {
            login_name: login.to_owned(),
            node_name: "laptop".to_owned(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    fn tailnet_addr() -> SocketAddr {
        "100.64.0.9:5555".parse().unwrap()
    }

    fn failure() -> tailscale::Error {
        tailscale::Error::Status {
            status: 404,
            body: "no such peer".to_owned(),
        }
    }

    fn allowed(decision: &Decision) -> bool {
        matches!(decision, Decision::Allow(_))
    }

    #[test]
    fn the_user_is_the_login_or_else_the_node() {
        assert_eq!(
            Peer::from(&who("a@example.com", &[])).user(),
            "a@example.com"
        );
        assert_eq!(Peer::anonymous("loopback").user(), "loopback");
    }

    #[test]
    fn empty_logins_mean_the_servers_own_login() {
        let policy = PeerPolicy::default();
        let me = who("ME@example.com", &[]);
        let other = who("you@example.com", &[]);
        assert!(allowed(&decide(
            Ok(&me),
            tailnet_addr(),
            &policy,
            Some("me@example.com")
        )));
        assert!(!allowed(&decide(
            Ok(&other),
            tailnet_addr(),
            &policy,
            Some("me@example.com")
        )));
        assert!(!allowed(&decide(Ok(&me), tailnet_addr(), &policy, None)));
    }

    #[test]
    fn listed_logins_replace_the_own_login() {
        let policy = PeerPolicy {
            logins: vec!["friend@example.com".to_owned()],
            ..PeerPolicy::default()
        };
        assert!(allowed(&decide(
            Ok(&who("Friend@Example.com", &[])),
            tailnet_addr(),
            &policy,
            Some("me@example.com")
        )));
        assert!(!allowed(&decide(
            Ok(&who("me@example.com", &[])),
            tailnet_addr(),
            &policy,
            Some("me@example.com")
        )));
    }

    #[test]
    fn tags_allow_tagged_nodes() {
        let policy = PeerPolicy {
            tags: vec!["tag:mesh".to_owned()],
            ..PeerPolicy::default()
        };
        let tagged = who("tagged-devices", &["tag:mesh", "tag:other"]);
        let wrong = who("tagged-devices", &["tag:other"]);
        assert_eq!(
            decide(Ok(&tagged), tailnet_addr(), &policy, Some("me@example.com")),
            Decision::Allow(Peer {
                node: "laptop".to_owned(),
                login: None,
            })
        );
        assert!(!allowed(&decide(
            Ok(&wrong),
            tailnet_addr(),
            &policy,
            Some("me@example.com")
        )));
    }

    #[tokio::test]
    async fn development_peers_are_distinguished_by_ip() {
        let checker = PeerChecker::new(&Config {
            insecure_skip_whois: true,
            ..Config::default()
        });
        let check = |address: &str| checker.check(address.parse().unwrap(), Role::Consumer);
        let first = check("100.64.0.1:1000").await;
        assert_eq!(first, check("[::ffff:100.64.0.1]:2000").await);
        assert_ne!(first, check("100.64.0.2:1000").await);
    }

    #[test]
    fn whois_errors_deny() {
        let policy = PeerPolicy::default();
        let error = failure();
        assert!(!allowed(&decide(
            Err(&error),
            tailnet_addr(),
            &policy,
            Some("me")
        )));
    }

    #[test]
    fn loopback_needs_the_flag() {
        let error = failure();
        let addr: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:4000".parse().unwrap();
        let off = PeerPolicy::default();
        let on = PeerPolicy {
            allow_loopback: true,
            ..PeerPolicy::default()
        };
        assert!(!allowed(&decide(Err(&error), addr, &off, None)));
        assert!(allowed(&decide(Err(&error), addr, &on, None)));
        assert!(allowed(&decide(Err(&error), mapped, &on, None)));
        assert!(!allowed(&decide(Err(&error), tailnet_addr(), &on, None)));
    }
}
