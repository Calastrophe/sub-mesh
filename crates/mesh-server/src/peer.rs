//! Who may talk to the server: tailnet identity checks.
//!
//! A caller is known by whois of its address. A request from a configured
//! trusted proxy is known by the login that the proxy names in a header
//! instead; see [`crate::config::TrustedProxy`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName};
use mesh_relay::tailscale::{self, LocalClient, WhoIs};
use tokio::sync::OnceCell;

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
/// Daemons dial the server directly, so their routes are refused through
/// the proxy. So are requests that a browser made from another site.
fn decide_proxied(
    headers: &HeaderMap,
    header: &HeaderName,
    role: Role,
    policy: &PeerPolicy,
    own_login: Option<&str>,
) -> Decision {
    if role == Role::Daemon {
        return Decision::Deny("daemon routes are not served through the trusted proxy".to_owned());
    }
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

/// Runs peer checks, with a short whois cache.
pub struct PeerChecker {
    client: LocalClient,
    skip_whois: bool,
    /// The trusted proxy's addresses, canonical. Never looked up by whois
    /// and never cached: one proxy address serves many callers.
    proxies: Vec<IpAddr>,
    proxy_header: HeaderName,
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
            proxies: config
                .trusted_proxy
                .addrs
                .iter()
                .map(IpAddr::to_canonical)
                .collect(),
            proxy_header: config.trusted_proxy.header_name(),
            consumers: config.consumers.clone(),
            daemons: config.daemons.clone(),
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

    /// Check the peer at `addr`, which sent `headers`, for `role`.
    pub async fn check(&self, addr: SocketAddr, headers: &HeaderMap, role: Role) -> Decision {
        let policy = match role {
            Role::Consumer => &self.consumers,
            Role::Daemon => &self.daemons,
        };
        if self.skip_whois {
            return Decision::Allow(Peer::anonymous(&addr.ip().to_canonical().to_string()));
        }
        if self.is_proxy(addr) {
            let own_login = if role == Role::Consumer && policy.logins.is_empty() {
                self.own_login().await
            } else {
                None
            };
            return decide_proxied(headers, &self.proxy_header, role, policy, own_login);
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn decide_via_proxy(headers: &HeaderMap, role: Role, policy: &PeerPolicy) -> Decision {
        let header = HeaderName::from_static(PROXY);
        decide_proxied(headers, &header, role, policy, Some("me@example.com"))
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
            decide_via_proxy(
                &proxied(&[b" Friend@Example.com "]),
                Role::Consumer,
                &friends()
            ),
            Decision::Allow(Peer {
                node: PROXY_NODE.to_owned(),
                login: Some("Friend@Example.com".to_owned()),
            })
        );
        assert!(!allowed(&decide_via_proxy(
            &proxied(&[b"stranger@example.com"]),
            Role::Consumer,
            &friends()
        )));
        let owner = PeerPolicy::default();
        assert!(allowed(&decide_via_proxy(
            &proxied(&[b"me@example.com"]),
            Role::Consumer,
            &owner
        )));
        assert!(!allowed(&decide_via_proxy(
            &proxied(&[b"friend@example.com"]),
            Role::Consumer,
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
                !allowed(&decide_via_proxy(&proxied(values), Role::Consumer, &policy)),
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
            assert!(!allowed(&decide_via_proxy(
                &headers,
                Role::Consumer,
                &tagged
            )));
            let owner = PeerPolicy::default();
            let own = Some("tagged-devices");
            let decision = decide_proxied(&headers, &header, Role::Consumer, &owner, own);
            assert!(!allowed(&decision));
        }
    }

    #[test]
    fn daemon_routes_are_refused_through_the_proxy() {
        let open = PeerPolicy {
            logins: vec!["friend@example.com".to_owned()],
            allow_loopback: true,
            ..PeerPolicy::default()
        };
        let headers = proxied(&[b"friend@example.com"]);
        assert!(!allowed(&decide_via_proxy(&headers, Role::Daemon, &open)));
    }

    #[test]
    fn pages_of_other_sites_cannot_use_the_proxy() {
        let with_site = |sites: &[&'static str]| {
            let mut headers = proxied(&[b"friend@example.com"]);
            for site in sites {
                headers.append("sec-fetch-site", axum::http::HeaderValue::from_static(site));
            }
            decide_via_proxy(&headers, Role::Consumer, &friends())
        };
        assert!(allowed(&with_site(&[])));
        assert!(allowed(&with_site(&["same-origin"])));
        assert!(allowed(&with_site(&["none"])));
        assert!(!allowed(&with_site(&["cross-site"])));
        assert!(!allowed(&with_site(&["same-site"])));
        assert!(!allowed(&with_site(&["same-origin", "cross-site"])));
    }

    /// A tailscaled that answers every whois with `login` and counts them.
    #[cfg(unix)]
    fn fake_tailscaled(login: &'static str) -> (std::path::PathBuf, std::sync::Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mesh-server-ts-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                    }
                    assert!(request.starts_with(b"GET /localapi/v0/whois?"));
                    counter.fetch_add(1, Ordering::SeqCst);
                    let body = format!(
                        r#"{{"Node":{{"Name":"laptop.ts.net.","ComputedName":"laptop"}},"UserProfile":{{"LoginName":"{login}"}}}}"#
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        (path, calls)
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
        assert!(!allowed(&checker.check(proxy, &friend, Role::Daemon).await));
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
