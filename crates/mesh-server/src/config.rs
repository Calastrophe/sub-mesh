//! Server configuration: the TOML file and its defaults.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::TimeDelta;
use mesh_core::protocol;
use mesh_relay::tailscale;
use serde::Deserialize;

/// System-wide config path, preferred when the file exists.
const SYSTEM_CONFIG: &str = "/etc/mesh/server.toml";

/// Where packages install the dashboard. Served when it exists and the
/// config does not name another directory.
const SYSTEM_WEB_DIR: &str = "/usr/share/mesh/web";

/// Who may use one group of routes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerPolicy {
    /// Tailnet logins that are allowed. Empty means the same login as the
    /// server's own machine.
    pub logins: Vec<String>,
    /// ACL tags that are allowed, such as `tag:mesh`.
    pub tags: Vec<String>,
    /// Allow connections from this machine without a tailnet identity.
    pub allow_loopback: bool,
}

/// The parsed server configuration.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub db_path: PathBuf,
    pub daemon_stale_secs: u64,
    /// How long the balancer assumes a provider keeps a prompt cache after
    /// its last use. A request that asks for longer overrides it: a Claude
    /// `cache_control` of one hour, or a Codex `prompt_cache_retention` of
    /// 24 hours.
    pub cache_ttl_secs: u32,
    /// The room an account needs before its reserve, as a fraction, to keep
    /// its sessions. Below this a session moves to another account.
    pub affinity_min_headroom: f64,
    pub max_attempts: usize,
    pub max_body_bytes: usize,
    pub tailscale_socket: PathBuf,
    /// Who may send consumer requests and use the management API.
    pub consumers: PeerPolicy,
    /// Who may send daemon reports.
    pub daemons: PeerPolicy,
    /// Skip every peer check. For development only.
    pub insecure_skip_whois: bool,
    /// The built dashboard, served at `/`. Defaults to `/usr/share/mesh/web`
    /// when that directory exists. An empty path turns the dashboard off.
    pub web_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from((Ipv4Addr::UNSPECIFIED, protocol::DEFAULT_SERVER_PORT)),
            db_path: default_db_path(),
            daemon_stale_secs: 60,
            cache_ttl_secs: 300,
            affinity_min_headroom: 0.02,
            max_attempts: 3,
            max_body_bytes: 64 * 1024 * 1024,
            tailscale_socket: PathBuf::from(tailscale::DEFAULT_SOCKET),
            consumers: PeerPolicy::default(),
            daemons: PeerPolicy::default(),
            insecure_skip_whois: false,
            web_dir: default_web_dir(),
        }
    }
}

impl Config {
    /// Parse and validate a TOML document.
    pub fn from_toml(text: &str) -> anyhow::Result<Config> {
        let mut config: Config = toml::from_str(text).context("invalid server config")?;
        config.web_dir = config.web_dir.filter(|dir| !dir.as_os_str().is_empty());
        config.validate()?;
        Ok(config)
    }

    /// Load the config from `explicit`, or from the default location.
    ///
    /// A missing explicit file is an error. When no file exists at the
    /// default locations, the defaults apply.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Config> {
        let path = match explicit {
            Some(path) => path.to_owned(),
            None => match default_config_path() {
                Some(path) if path.exists() => path,
                _ => return Ok(Config::default()),
            },
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Config::from_toml(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn daemon_stale(&self) -> Duration {
        Duration::from_secs(self.daemon_stale_secs)
    }

    pub fn cache_ttl(&self) -> TimeDelta {
        TimeDelta::seconds(self.cache_ttl_secs.into())
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.max_attempts == 0 {
            bail!("max_attempts must be at least 1");
        }
        if self.daemon_stale_secs == 0 {
            bail!("daemon_stale_secs must be at least 1");
        }
        if !(0.0..=1.0).contains(&self.affinity_min_headroom) {
            bail!("affinity_min_headroom must be between 0 and 1");
        }
        if let Some(dir) = &self.web_dir {
            if !dir.is_dir() {
                bail!("web_dir {} is not a directory", dir.display());
            }
            if !dir.join("index.html").is_file() {
                bail!("web_dir {} has no index.html", dir.display());
            }
        }
        Ok(())
    }
}

fn default_config_path() -> Option<PathBuf> {
    let system = PathBuf::from(SYSTEM_CONFIG);
    if system.exists() {
        return Some(system);
    }
    dirs::config_dir().map(|dir| dir.join("mesh/server.toml"))
}

fn default_web_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(SYSTEM_WEB_DIR);
    dir.is_dir().then_some(dir)
}

fn default_db_path() -> PathBuf {
    db_path_from(
        std::env::var("STATE_DIRECTORY").ok().as_deref(),
        running_as_root(),
        dirs::data_local_dir(),
    )
}

/// Pick the default database path. systemd's `STATE_DIRECTORY` wins, then
/// the system path for root, then the user's data directory.
fn db_path_from(state_dir: Option<&str>, root: bool, data_dir: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = state_dir
        .and_then(|dirs| std::env::split_paths(dirs).next())
        .filter(|dir| !dir.as_os_str().is_empty())
    {
        return dir.join("server.db");
    }
    if root {
        return PathBuf::from("/var/lib/mesh/server.db");
    }
    data_dir
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mesh/server.db")
}

fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|rest| rest.split_whitespace().nth(1).map(|euid| euid == "0"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn empty_file_gives_defaults() {
        let config = Config::from_toml("").expect("parse");
        assert_eq!(config.listen.port(), protocol::DEFAULT_SERVER_PORT);
        assert_eq!(config.daemon_stale_secs, 60);
        assert_eq!(config.cache_ttl_secs, 300);
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.max_body_bytes, 64 * 1024 * 1024);
        assert_eq!(
            config.tailscale_socket,
            PathBuf::from(tailscale::DEFAULT_SOCKET)
        );
        assert_eq!(config.affinity_min_headroom, 0.02);
        assert_eq!(config.consumers, PeerPolicy::default());
        assert!(!config.daemons.allow_loopback);
        assert!(!config.insecure_skip_whois);
    }

    #[test]
    fn partial_file_overrides_only_given_fields() {
        let config = Config::from_toml(
            r#"
            listen = "127.0.0.1:9000"
            max_attempts = 5
            [consumers]
            logins = ["a@example.com"]
            allow_loopback = true
            [daemons]
            tags = ["tag:mesh"]
            "#,
        )
        .expect("parse");
        assert_eq!(config.listen, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.daemon_stale_secs, 60);
        assert_eq!(config.consumers.logins, ["a@example.com"]);
        assert!(config.consumers.allow_loopback);
        assert_eq!(config.daemons.tags, ["tag:mesh"]);
        assert!(config.daemons.logins.is_empty());
    }

    #[test]
    fn rejects_unknown_fields_and_bad_values() {
        assert!(Config::from_toml("lisen = \"x\"").is_err());
        assert!(Config::from_toml("max_attempts = 0").is_err());
        assert!(Config::from_toml("daemon_stale_secs = 0").is_err());
    }

    #[test]
    fn affinity_min_headroom_must_be_a_fraction() {
        assert_eq!(
            Config::from_toml("affinity_min_headroom = 0.1")
                .expect("parse")
                .affinity_min_headroom,
            0.1
        );
        for bad in ["-0.1", "1.5", "nan"] {
            let text = format!("affinity_min_headroom = {bad}");
            assert!(Config::from_toml(&text).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn cache_ttl_is_in_seconds_and_the_old_affinity_ttl_is_gone() {
        let config = Config::from_toml("cache_ttl_secs = 600").expect("parse");
        assert_eq!(config.cache_ttl(), TimeDelta::minutes(10));
        assert!(Config::from_toml("affinity_ttl_secs = 3600").is_err());
        assert!(Config::from_toml("cache_ttl_secs = -1").is_err());
    }

    #[test]
    fn users_tables_are_gone() {
        assert!(Config::from_toml("[[users]]\nlogin = \"a\"").is_err());
    }

    fn site_toml(dir: &TempDir) -> String {
        format!("web_dir = {:?}", dir.path().display().to_string())
    }

    #[test]
    fn web_dir_must_hold_a_site() {
        let site = TempDir::new();
        site.write("index.html", "<html></html>");
        let config = Config::from_toml(&site_toml(&site)).expect("parse");
        assert_eq!(config.web_dir.as_deref(), Some(site.path()));

        let empty = TempDir::new();
        let error = format!("{:#}", Config::from_toml(&site_toml(&empty)).unwrap_err());
        assert!(error.contains("no index.html"), "{error}");

        let missing = "web_dir = \"/nonexistent/mesh-web\"";
        let error = format!("{:#}", Config::from_toml(missing).unwrap_err());
        assert!(error.contains("not a directory"), "{error}");
    }

    #[test]
    fn empty_web_dir_turns_the_dashboard_off() {
        let config = Config::from_toml("web_dir = \"\"").expect("parse");
        assert_eq!(config.web_dir, None);
    }

    #[test]
    fn db_path_choice() {
        let data = Some(PathBuf::from("/home/u/.local/share"));
        assert_eq!(
            db_path_from(Some("/var/lib/mesh"), false, data.clone()),
            PathBuf::from("/var/lib/mesh/server.db")
        );
        assert_eq!(
            db_path_from(None, true, data.clone()),
            PathBuf::from("/var/lib/mesh/server.db")
        );
        assert_eq!(
            db_path_from(None, false, data),
            PathBuf::from("/home/u/.local/share/mesh/server.db")
        );
    }
}
