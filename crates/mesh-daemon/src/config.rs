//! Daemon configuration: the TOML file as written, and the resolved settings
//! the rest of the daemon uses.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mesh_core::protocol::DEFAULT_ADMIN_PORT;
use mesh_core::{ModelAllowlist, Provider, ReserveConfig};
use mesh_providers::Endpoints;
use mesh_relay::tailscale::{self, SelfStatus};
use serde::Deserialize;
use url::Url;

use crate::peer;

/// The config file as written by the user.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server_url: String,
    pub node: Option<String>,
    pub admin_listen: Option<SocketAddr>,
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub reserve: ReserveConfig,
    /// The `[models]` table: the model allowlist for accounts of each
    /// provider that have none of their own, such as
    /// `claude = ["claude-sonnet-*"]`. A provider that is missing allows
    /// every model.
    #[serde(default)]
    pub models: BTreeMap<Provider, ModelAllowlist>,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_report_interval_secs")]
    pub report_interval_secs: u64,
    #[serde(default = "default_tailscale_socket")]
    pub tailscale_socket: PathBuf,
    #[serde(default)]
    pub allowed_peers: PeerPolicy,
    /// Extra tailnet logins that may manage this machine's subscriptions from
    /// the dashboard (reserve, pause, remove), besides this machine's own
    /// login. Matched ignoring case. This is separate from `allowed_peers`,
    /// which decides which server the daemon trusts to open its tunnel.
    #[serde(default)]
    pub owners: Vec<String>,
    /// Development overrides, for example `[endpoints.codex]`.
    #[serde(default)]
    endpoints: BTreeMap<Provider, EndpointOverrides>,
    /// Development only: trust every server without asking tailscaled.
    #[serde(default)]
    pub insecure_skip_whois: bool,
}

fn default_poll_interval_secs() -> u64 {
    90
}

fn default_report_interval_secs() -> u64 {
    15
}

fn default_tailscale_socket() -> PathBuf {
    PathBuf::from(tailscale::DEFAULT_SOCKET)
}

/// The `[allowed_peers]` table: which servers the daemon trusts. The daemon
/// checks the tailnet identity of the host behind `server_url` before it
/// opens its tunnel, and sends nothing to a host that fails the check.
///
/// This applies to an `http://` `server_url` only, and must be left empty
/// for an `https://` one, whose TLS certificate for the URL's host
/// identifies the server instead.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerPolicy {
    /// Logins the server may run as. Empty means "the same login as this
    /// machine".
    pub logins: Vec<String>,
    /// ACL tags the server may carry, such as `tag:mesh`.
    pub tags: Vec<String>,
    /// Trust a server on a loopback address without a tailnet identity.
    pub allow_loopback: bool,
    #[serde(skip)]
    pub skip_whois: bool,
}

/// Each omitted URL keeps its provider default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointOverrides {
    upstream_base: Option<Url>,
    token_url: Option<Url>,
    usage_url: Option<Url>,
}

impl EndpointOverrides {
    fn apply(self, endpoints: &mut Endpoints) -> Result<()> {
        for (name, target, value) in [
            (
                "upstream_base",
                &mut endpoints.upstream_base,
                self.upstream_base,
            ),
            ("token_url", &mut endpoints.token_url, self.token_url),
            ("usage_url", &mut endpoints.usage_url, self.usage_url),
        ] {
            if let Some(url) = value {
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    bail!(
                        "endpoints.{}.{name} must be an HTTP(S) URL without user info or a fragment",
                        endpoints.provider
                    );
                }
                *target = url;
            }
        }
        Ok(())
    }
}

/// Settings after defaults are filled in.
#[derive(Clone, Debug)]
pub struct Settings {
    pub server_url: String,
    /// `server_url` parsed: the host the daemon dials.
    pub server: Url,
    pub node: String,
    pub admin_listen: SocketAddr,
    pub data_dir: PathBuf,
    pub reserve: ReserveConfig,
    /// The model allowlist for accounts without their own, by provider (see
    /// [`Config::models`]), in provider order.
    pub models: BTreeMap<Provider, ModelAllowlist>,
    pub poll_interval: Duration,
    pub report_interval: Duration,
    pub tailscale_socket: PathBuf,
    pub peers: PeerPolicy,
    pub initial_owner: Option<String>,
    /// Extra logins that may manage subscriptions (see [`Config::owners`]).
    pub owners: Vec<String>,
    endpoints: BTreeMap<Provider, Endpoints>,
}

impl Settings {
    pub fn endpoints(&self, provider: Provider) -> &Endpoints {
        self.endpoints
            .get(&provider)
            .expect("all providers have endpoints")
    }

    /// Where account token files live.
    pub fn accounts_dir(&self) -> PathBuf {
        self.data_dir.join("accounts")
    }

    /// Where the admin API token lives.
    pub fn admin_token_path(&self) -> PathBuf {
        self.data_dir.join(mesh_core::admin::TOKEN_FILE)
    }

    /// Where runtime settings (pauses, reserve and model overrides) are saved.
    pub fn runtime_path(&self) -> PathBuf {
        self.data_dir.join("daemon-state.json")
    }
}

/// The default config file location.
pub fn default_config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("cannot find the user config directory")?;
    Ok(dir.join("mesh").join("daemon.toml"))
}

impl Config {
    /// Read and parse a config file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        text.parse()
            .with_context(|| format!("invalid config {}", path.display()))
    }

    /// Fill in defaults. `own` is this machine's tailnet identity when known;
    /// `hostname` is the system host name.
    pub fn resolve(self, own: Option<&SelfStatus>, hostname: &str) -> Result<Settings> {
        let server_url = self.server_url.trim_end_matches('/').to_owned();
        let parsed = Url::parse(&server_url).context("server_url is not a valid URL")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            bail!("server_url must be an http:// or https:// origin without credentials");
        }
        let peers = &self.allowed_peers;
        if parsed.scheme() == "https"
            && (!peers.logins.is_empty() || !peers.tags.is_empty() || peers.allow_loopback)
        {
            bail!(
                "allowed_peers applies to an http:// server_url only: \
                 an https:// server is identified by its TLS certificate"
            );
        }

        let admin_listen = self
            .admin_listen
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], DEFAULT_ADMIN_PORT)));
        if !admin_listen.ip().is_loopback() {
            bail!("admin_listen {admin_listen} must be a loopback address");
        }

        let node = self
            .node
            .map(|node| node.trim().to_owned())
            .or_else(|| {
                own.map(|status| status.host_name.clone())
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or_else(|| hostname.to_owned());
        if !mesh_core::protocol::valid_node_name(&node) {
            bail!(
                "node {node:?} is not a valid node name: use at most {} bytes and no control characters",
                mesh_core::protocol::MAX_NODE_NAME
            );
        }

        let data_dir = match self.data_dir {
            Some(dir) => dir,
            None => {
                mesh_providers::default_data_dir().context("cannot find the user data directory")?
            }
        };

        if self.poll_interval_secs == 0 || self.report_interval_secs == 0 {
            bail!("poll_interval_secs and report_interval_secs must be greater than zero");
        }

        let peers = PeerPolicy {
            skip_whois: self.insecure_skip_whois,
            ..self.allowed_peers
        };

        let mut overrides = self.endpoints;
        let mut endpoints = BTreeMap::new();
        for provider in Provider::ALL {
            let mut resolved = Endpoints::new(provider);
            if let Some(overrides) = overrides.remove(&provider) {
                overrides.apply(&mut resolved)?;
            }
            endpoints.insert(provider, resolved);
        }

        Ok(Settings {
            server_url,
            server: parsed,
            node,
            admin_listen,
            data_dir,
            reserve: self.reserve,
            models: self.models,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            report_interval: Duration::from_secs(self.report_interval_secs),
            tailscale_socket: self.tailscale_socket,
            peers,
            initial_owner: own
                .and_then(|status| peer::owner_login(&status.login_name))
                .map(str::to_owned),
            owners: self.owners,
            endpoints,
        })
    }
}

impl std::str::FromStr for Config {
    type Err = toml::de::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        toml::from_str(text)
    }
}

/// The system host name (from `COMPUTERNAME` on Windows), or `localhost` when
/// it cannot be read.
pub fn system_hostname() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.trim().to_owned())
        .find(|name| !name.is_empty())
        .or_else(|| {
            ["HOSTNAME", "COMPUTERNAME"]
                .iter()
                .filter_map(|name| std::env::var(name).ok())
                .find(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_owned())
}

#[cfg(test)]
mod tests;
