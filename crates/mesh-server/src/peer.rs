//! Who may talk to the server: tailnet identity checks.
//!
//! A caller is known by whois of its address. A request from a configured
//! trusted proxy is known by the login that the proxy names in a header
//! instead; see [`crate::config::TrustedProxy`].

use std::borrow::Cow;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName};
use mesh_relay::tailscale::{self, LocalClient, WhoIs};
use tokio::sync::{OnceCell, watch};

use crate::config::{Config, PeerPolicy};

/// How long a successful whois answer is reused.
const WHOIS_TTL: Duration = Duration::from_secs(60);
const CACHE_LIMIT: usize = 256;

/// The node name of callers that the trusted proxy identified. Whois would
/// name the proxy's machine, not theirs.
pub const PROXY_NODE: &str = "via-proxy";
/// The longest login taken from the trusted proxy's header.
const MAX_PROXY_LOGIN: usize = 254;

/// Which set of routes a request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Consumer routes and the management API.
    Consumer,
    /// The daemon routes: reports and the tunnel.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    Open,
    Loopback,
    Who { login: String, tags: Vec<String> },
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
    match who {
        Ok(who) => admit(who, policy, own_login),
        Err(error) => Decision::Deny(format!("whois failed for {addr}: {error}")),
    }
}

/// Decide whether the caller `who` may pass `policy`, by its login or tags.
fn admit(who: &WhoIs, policy: &PeerPolicy, own_login: Option<&str>) -> Decision {
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

/// The login that the trusted proxy names in `header`. A request must carry
/// exactly one plausible login; the proxy sets the header for every request
/// it identified.
fn proxy_login<'a>(headers: &'a HeaderMap, header: &HeaderName) -> Result<&'a str, String> {
    let mut values = headers.get_all(header).iter();
    let value = match (values.next(), values.next()) {
        (Some(value), None) => value,
        (None, _) => return Err(format!("the trusted proxy sent no {header} header")),
        _ => {
            return Err(format!(
                "the trusted proxy sent more than one {header} header"
            ));
        }
    };
    let login = std::str::from_utf8(value.as_bytes())
        .map_err(|_| format!("the {header} header is not UTF-8"))?
        .trim();
    if login.is_empty() {
        return Err(format!("the {header} header is empty"));
    }
    if login.len() > MAX_PROXY_LOGIN {
        return Err(format!("the {header} header is too long"));
    }
    if login.chars().any(char::is_control) {
        return Err(format!("the {header} header has control characters"));
    }
    if login.eq_ignore_ascii_case(TAGGED_LOGIN) {
        return Err(format!("the {header} header names no user"));
    }
    Ok(login)
}

/// Whether a browser says that a page of another site made the request.
/// Through the proxy the browser's own tailnet identity is attached to any
/// request it makes, as it is for direct tailnet callers.
fn cross_site(headers: &HeaderMap) -> bool {
    let mut sites = headers.get_all("sec-fetch-site").iter();
    match (sites.next(), sites.next()) {
        (None, _) => false,
        (Some(site), None) => !matches!(site.as_bytes(), b"same-origin" | b"none"),
        _ => true,
    }
}

/// Decide whether a request that came through the trusted proxy may pass
/// `policy`, as if whois had named the login in its header.
///
/// This holds for daemon routes as for consumer routes: a daemon gets the
/// rights its login would have by whois. Its node name, from the tunnel
/// upgrade or its report, is only a label under that login. Requests that
/// a browser made from another site are refused.
fn decide_proxied(
    headers: &HeaderMap,
    header: &HeaderName,
    policy: &PeerPolicy,
    own_login: Option<&str>,
) -> Decision {
    let login = match proxy_login(headers, header) {
        Ok(login) => login,
        Err(reason) => return Decision::Deny(reason),
    };
    if cross_site(headers) {
        return Decision::Deny(format!(
            "a page of another site sent a request as {login} through the trusted proxy"
        ));
    }
    let who = WhoIs {
        login_name: login.to_owned(),
        node_name: PROXY_NODE.to_owned(),
        tags: Vec::new(),
    };
    admit(&who, policy, own_login)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Policies {
    pub consumers: PeerPolicy,
    pub daemons: PeerPolicy,
    pub admins: Vec<String>,
    pub own_consumers: bool,
    pub own_daemons: bool,
}

impl Policies {
    pub fn from_config(config: &Config) -> Policies {
        let mut consumers = config.consumers.clone();
        merge_logins(&mut consumers.logins, &config.admins.logins);
        Policies {
            consumers,
            daemons: config.daemons.clone(),
            admins: config.admins.logins.clone(),
            own_consumers: config.consumers.logins.is_empty(),
            own_daemons: config.daemons.logins.is_empty(),
        }
    }

    fn for_role(&self, role: Role) -> (&PeerPolicy, bool) {
        match role {
            Role::Consumer => (&self.consumers, self.own_consumers),
            Role::Daemon => (&self.daemons, self.own_daemons),
        }
    }

    pub fn is_admin(&self, login: &str) -> bool {
        self.admins
            .iter()
            .any(|admin| admin.eq_ignore_ascii_case(login))
    }
}

pub fn merge_logins(into: &mut Vec<String>, more: &[String]) {
    for login in more {
        if !into.iter().any(|known| known.eq_ignore_ascii_case(login)) {
            into.push(login.clone());
        }
    }
}

pub fn merge_tags(into: &mut Vec<String>, more: &[String]) {
    for tag in more {
        if !into.contains(tag) {
            into.push(tag.clone());
        }
    }
}

fn with_own<'a>(policy: &'a PeerPolicy, own_login: Option<&str>) -> Cow<'a, PeerPolicy> {
    match own_login {
        Some(own) if !policy.logins.is_empty() => {
            let mut policy = policy.clone();
            merge_logins(&mut policy.logins, &[own.to_owned()]);
            Cow::Owned(policy)
        }
        _ => Cow::Borrowed(policy),
    }
}

/// Runs peer checks, with a short whois cache.
pub struct PeerChecker {
    client: LocalClient,
    skip_whois: bool,
    /// The trusted proxy's addresses, canonical. Never looked up by whois
    /// and never cached: one proxy address serves many callers.
    proxies: Vec<IpAddr>,
    proxy_header: HeaderName,
    /// Only the trusted proxy may call; whois is never asked.
    proxy_only: bool,
    policies: RwLock<Arc<Policies>>,
    revision: watch::Sender<u64>,
    own_login: OnceCell<String>,
    cache: Mutex<HashMap<IpAddr, (Instant, WhoIs)>>,
}

impl PeerChecker {
    pub fn new(config: &Config) -> PeerChecker {
        PeerChecker {
            client: LocalClient::new(config.tailscale_socket.clone()),
            skip_whois: config.insecure_skip_whois,
            proxies: config
                .trusted_proxy
                .addrs
                .iter()
                .map(IpAddr::to_canonical)
                .collect(),
            proxy_header: config.trusted_proxy.header_name(),
            proxy_only: config.trusted_proxy.only,
            policies: RwLock::new(Arc::new(Policies::from_config(config))),
            revision: watch::Sender::new(0),
            own_login: OnceCell::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The header that names the caller's login on requests from the
    /// trusted proxy. It must not reach daemons or providers.
    pub fn identity_header(&self) -> &HeaderName {
        &self.proxy_header
    }

    fn is_proxy(&self, addr: SocketAddr) -> bool {
        self.proxies.contains(&addr.ip().to_canonical())
    }

    pub fn policies(&self) -> Arc<Policies> {
        self.policies
            .read()
            .expect("policies lock poisoned")
            .clone()
    }

    pub fn set_policies(&self, policies: Policies) {
        *self.policies.write().expect("policies lock poisoned") = Arc::new(policies);
        self.revision.send_modify(|revision| *revision += 1);
    }

    pub fn skips_whois(&self) -> bool {
        self.skip_whois
    }

    pub fn is_admin(&self, peer: &Peer) -> bool {
        self.skip_whois
            || peer
                .login
                .as_deref()
                .is_some_and(|login| self.policies().is_admin(login))
    }

    /// Check the peer at `addr`, which sent `headers`, for `role`.
    pub async fn check(&self, addr: SocketAddr, headers: &HeaderMap, role: Role) -> Decision {
        match self.check_grant(addr, headers, role).await {
            Ok((peer, _)) => Decision::Allow(peer),
            Err(reason) => Decision::Deny(reason),
        }
    }

    pub async fn check_grant(
        &self,
        addr: SocketAddr,
        headers: &HeaderMap,
        role: Role,
    ) -> Result<(Peer, Grant), String> {
        let policies = self.policies();
        let (policy, own_fallback) = policies.for_role(role);
        if self.skip_whois {
            return Ok((
                Peer::anonymous(&addr.ip().to_canonical().to_string()),
                Grant::Open,
            ));
        }
        if self.is_proxy(addr) {
            let own_login = if own_fallback {
                self.own_login().await
            } else {
                None
            };
            let policy = with_own(policy, own_login);
            return match decide_proxied(headers, &self.proxy_header, &policy, own_login) {
                Decision::Allow(peer) => {
                    let grant = Grant::Who {
                        login: peer.login.clone().unwrap_or_default(),
                        tags: Vec::new(),
                    };
                    Ok((peer, grant))
                }
                Decision::Deny(reason) => Err(reason),
            };
        }
        if loopback_allowed(addr, policy) {
            return Ok((Peer::anonymous("loopback"), Grant::Loopback));
        }
        if self.proxy_only {
            return Err(format!(
                "{addr} is not the trusted proxy, and trusted_proxy.only is set"
            ));
        }
        let own_login = if own_fallback {
            self.own_login().await
        } else {
            None
        };
        let policy = with_own(policy, own_login);
        let who = self.whois(addr).await;
        match decide(who.as_ref(), addr, &policy, own_login) {
            Decision::Allow(peer) => {
                let grant = match &who {
                    Ok(who) => Grant::Who {
                        login: who.login_name.trim().to_owned(),
                        tags: who.tags.clone(),
                    },
                    Err(_) => Grant::Loopback,
                };
                Ok((peer, grant))
            }
            Decision::Deny(reason) => Err(reason),
        }
    }

    pub fn admits(&self, grant: &Grant, role: Role) -> bool {
        if self.skip_whois {
            return true;
        }
        let policies = self.policies();
        let (policy, own_fallback) = policies.for_role(role);
        match grant {
            Grant::Open => true,
            Grant::Loopback => policy.allow_loopback,
            Grant::Who { login, tags } => {
                let own_login = if own_fallback {
                    self.own_login.get().map(String::as_str)
                } else {
                    None
                };
                let policy = with_own(policy, own_login);
                let who = WhoIs {
                    login_name: login.clone(),
                    node_name: String::new(),
                    tags: tags.clone(),
                };
                let admitted = matches!(admit(&who, &policy, own_login), Decision::Allow(_));
                admitted || (own_fallback && self.own_login.get().is_none())
            }
        }
    }

    pub fn admits_login(&self, login: &str, role: Role) -> bool {
        self.admits(
            &Grant::Who {
                login: login.to_owned(),
                tags: Vec::new(),
            },
            role,
        )
    }

    pub fn changes(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub async fn identify(&self, addr: SocketAddr, headers: &HeaderMap) -> Result<Peer, String> {
        if self.skip_whois {
            return Err("insecure_skip_whois leaves callers without a login".to_owned());
        }
        if self.is_proxy(addr) {
            let login = proxy_login(headers, &self.proxy_header)?;
            if cross_site(headers) {
                return Err(format!(
                    "a page of another site sent a request as {login} through the trusted proxy"
                ));
            }
            return Ok(Peer {
                node: PROXY_NODE.to_owned(),
                login: Some(login.to_owned()),
            });
        }
        if self.proxy_only {
            return Err(format!(
                "{addr} is not the trusted proxy, and trusted_proxy.only is set"
            ));
        }
        let who = self
            .whois(addr)
            .await
            .map_err(|error| format!("whois failed for {addr}: {error}"))?;
        let peer = Peer::from(&who);
        if peer.login.is_none() {
            return Err(format!("{} has no user login", who.node_name));
        }
        Ok(peer)
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
    use std::sync::atomic::Ordering;

    use super::*;
    #[cfg(unix)]
    use crate::testing::fake_tailscaled;

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
        let headers = HeaderMap::new();
        let check =
            |address: &str| checker.check(address.parse().unwrap(), &headers, Role::Consumer);
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

    const PROXY: &str = "x-webauth-user";

    fn proxied(values: &[&[u8]]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(PROXY, axum::http::HeaderValue::from_bytes(value).unwrap());
        }
        headers
    }

    fn decide_via_proxy(headers: &HeaderMap, policy: &PeerPolicy) -> Decision {
        let header = HeaderName::from_static(PROXY);
        decide_proxied(headers, &header, policy, Some("me@example.com"))
    }

    fn friends() -> PeerPolicy {
        PeerPolicy {
            logins: vec!["friend@example.com".to_owned()],
            ..PeerPolicy::default()
        }
    }

    #[test]
    fn the_proxy_header_names_the_login() {
        assert_eq!(
            decide_via_proxy(&proxied(&[b" Friend@Example.com "]), &friends()),
            Decision::Allow(Peer {
                node: PROXY_NODE.to_owned(),
                login: Some("Friend@Example.com".to_owned()),
            })
        );
        assert!(!allowed(&decide_via_proxy(
            &proxied(&[b"stranger@example.com"]),
            &friends()
        )));
        let owner = PeerPolicy::default();
        assert!(allowed(&decide_via_proxy(
            &proxied(&[b"me@example.com"]),
            &owner
        )));
        assert!(!allowed(&decide_via_proxy(
            &proxied(&[b"friend@example.com"]),
            &owner
        )));
    }

    #[test]
    fn implausible_proxy_headers_deny() {
        let long = format!("{}@example.com", "a".repeat(MAX_PROXY_LOGIN));
        let mut policy = friends();
        policy.logins.push(long.clone());
        for values in [
            &[][..],
            &[&b""[..]][..],
            &[&b"   "[..]][..],
            &[&b"friend@example.com"[..], &b"friend@example.com"[..]][..],
            &[&b"friend@example.com"[..], &b""[..]][..],
            &[long.as_bytes()][..],
            &[&b"friend@example.com\tx"[..]][..],
            &[&b"friend@example.com\xff"[..]][..],
        ] {
            assert!(
                !allowed(&decide_via_proxy(&proxied(values), &policy)),
                "{values:?}"
            );
        }
    }

    #[test]
    fn the_proxy_cannot_claim_a_tagged_node() {
        let tagged = PeerPolicy {
            logins: vec!["tagged-devices".to_owned()],
            tags: vec!["tag:mesh".to_owned()],
            ..PeerPolicy::default()
        };
        let header = HeaderName::from_static(PROXY);
        for value in [&b"tagged-devices"[..], b"Tagged-Devices"] {
            let headers = proxied(&[value]);
            assert!(!allowed(&decide_via_proxy(&headers, &tagged)));
            let owner = PeerPolicy::default();
            let own = Some("tagged-devices");
            let decision = decide_proxied(&headers, &header, &owner, own);
            assert!(!allowed(&decision));
        }
    }

    #[test]
    fn a_daemon_through_the_proxy_has_the_rights_of_its_login() {
        let daemons = friends();
        assert_eq!(
            decide_via_proxy(&proxied(&[b"friend@example.com"]), &daemons),
            Decision::Allow(Peer {
                node: PROXY_NODE.to_owned(),
                login: Some("friend@example.com".to_owned()),
            })
        );
        assert!(!allowed(&decide_via_proxy(
            &proxied(&[b"stranger@example.com"]),
            &daemons
        )));
        let tagged = PeerPolicy {
            tags: vec!["tag:mesh".to_owned()],
            ..PeerPolicy::default()
        };
        assert!(!allowed(&decide_via_proxy(
            &proxied(&[b"friend@example.com"]),
            &tagged
        )));
    }

    #[test]
    fn pages_of_other_sites_cannot_use_the_proxy() {
        let with_site = |sites: &[&'static str]| {
            let mut headers = proxied(&[b"friend@example.com"]);
            for site in sites {
                headers.append("sec-fetch-site", axum::http::HeaderValue::from_static(site));
            }
            decide_via_proxy(&headers, &friends())
        };
        assert!(allowed(&with_site(&[])));
        assert!(allowed(&with_site(&["same-origin"])));
        assert!(allowed(&with_site(&["none"])));
        assert!(!allowed(&with_site(&["cross-site"])));
        assert!(!allowed(&with_site(&["same-site"])));
        assert!(!allowed(&with_site(&["same-origin", "cross-site"])));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_proxy_address_is_trusted_and_it_is_never_looked_up() {
        let (socket, whois_calls) = fake_tailscaled("friend@example.com");
        let checker = PeerChecker::new(&Config {
            tailscale_socket: socket.clone(),
            consumers: friends(),
            daemons: friends(),
            trusted_proxy: crate::config::TrustedProxy {
                addrs: vec!["10.10.10.111".parse().unwrap()],
                ..Default::default()
            },
            ..Config::default()
        });
        let proxy: SocketAddr = "10.10.10.111:40000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:10.10.10.111]:40001".parse().unwrap();
        let via_proxy = Decision::Allow(Peer {
            node: PROXY_NODE.to_owned(),
            login: Some("friend@example.com".to_owned()),
        });
        let friend = proxied(&[b"friend@example.com"]);
        assert_eq!(
            checker.check(proxy, &friend, Role::Consumer).await,
            via_proxy
        );
        assert_eq!(
            checker.check(mapped, &friend, Role::Consumer).await,
            via_proxy
        );
        let stranger = proxied(&[b"stranger@example.com"]);
        assert!(!allowed(
            &checker.check(proxy, &stranger, Role::Consumer).await
        ));
        assert!(!allowed(
            &checker
                .check(proxy, &HeaderMap::new(), Role::Consumer)
                .await
        ));
        assert_eq!(checker.check(proxy, &friend, Role::Daemon).await, via_proxy);
        assert!(!allowed(
            &checker.check(proxy, &stranger, Role::Daemon).await
        ));
        assert_eq!(whois_calls.load(Ordering::SeqCst), 0);
        assert!(checker.cache.lock().unwrap().is_empty());

        let direct: SocketAddr = "100.64.0.9:5555".parse().unwrap();
        assert_eq!(
            checker.check(direct, &stranger, Role::Consumer).await,
            Decision::Allow(Peer {
                node: "laptop".to_owned(),
                login: Some("friend@example.com".to_owned()),
            })
        );
        assert_eq!(whois_calls.load(Ordering::SeqCst), 1);
        assert_eq!(checker.cache.lock().unwrap().len(), 1);
        assert!(!checker.cache.lock().unwrap().contains_key(&proxy.ip()));
        let _ = std::fs::remove_file(socket);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_header_counts_for_nothing_from_other_addresses() {
        let (socket, whois_calls) = fake_tailscaled("stranger@example.com");
        let checker = PeerChecker::new(&Config {
            tailscale_socket: socket.clone(),
            consumers: PeerPolicy {
                logins: vec!["friend@example.com".to_owned()],
                allow_loopback: true,
                ..PeerPolicy::default()
            },
            trusted_proxy: crate::config::TrustedProxy {
                addrs: vec!["10.10.10.111".parse().unwrap()],
                ..Default::default()
            },
            ..Config::default()
        });
        let friend = proxied(&[b"friend@example.com"]);
        let direct: SocketAddr = "100.64.0.9:5555".parse().unwrap();
        assert!(!allowed(
            &checker.check(direct, &friend, Role::Consumer).await
        ));
        assert_eq!(whois_calls.load(Ordering::SeqCst), 1);
        let loopback: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        assert_eq!(
            checker.check(loopback, &friend, Role::Consumer).await,
            Decision::Allow(Peer::anonymous("loopback"))
        );
        let _ = std::fs::remove_file(socket);
    }

    #[tokio::test]
    async fn a_proxy_only_server_never_asks_whois() {
        let checker = PeerChecker::new(&Config {
            tailscale_socket: "/nonexistent/tailscaled.sock".into(),
            consumers: friends(),
            daemons: friends(),
            trusted_proxy: crate::config::TrustedProxy {
                addrs: vec![
                    "10.10.10.111".parse().unwrap(),
                    "127.0.0.2".parse().unwrap(),
                ],
                only: true,
                ..Default::default()
            },
            ..Config::default()
        });
        let friend = proxied(&[b"friend@example.com"]);
        let proxy: SocketAddr = "10.10.10.111:40000".parse().unwrap();
        for role in [Role::Consumer, Role::Daemon] {
            assert!(allowed(&checker.check(proxy, &friend, role).await));
        }
        let direct: SocketAddr = "100.64.0.9:5555".parse().unwrap();
        let Decision::Deny(reason) = checker.check(direct, &friend, Role::Daemon).await else {
            panic!("a direct caller was allowed");
        };
        assert!(reason.contains("trusted_proxy.only"), "{reason}");
        let loopback: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let local_proxy: SocketAddr = "127.0.0.2:40000".parse().unwrap();
        for role in [Role::Consumer, Role::Daemon] {
            assert!(!allowed(&checker.check(loopback, &friend, role).await));
            assert!(allowed(&checker.check(local_proxy, &friend, role).await));
        }
        assert!(checker.cache.lock().unwrap().is_empty());
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
