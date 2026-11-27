use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use toml::{Table, Value};
use url::Url;

pub(crate) fn run(
    server_url: Option<String>,
    tailscale_peer: Option<String>,
    yes: bool,
) -> Result<()> {
    let server_url = match server_url {
        Some(url) => url,
        None => prompt("Mesh server URL (for example https://mesh.graybird.software): ")?,
    };
    let server_url = validate_url(&server_url)?;

    let peer = if server_url.starts_with("https://") {
        if tailscale_peer.is_some() {
            bail!(
                "--tailscale-peer applies to HTTP over Tailscale; HTTPS uses its TLS certificate for server identity"
            );
        }
        None
    } else {
        match tailscale_peer {
            Some(peer) => Some(peer),
            None if yes => None,
            None => {
                eprintln!(
                    "For HTTP over Tailscale, mesh verifies the server's Tailscale identity.\n\
                     Leave this blank when the server uses the same Tailscale login as this machine.\n\
                     Enter its login (you@example.com) or ACL tag (tag:mesh) if it uses another identity."
                );
                prompt_optional("Server Tailscale login or tag [same login]: ")?
            }
        }
    };

    let path = config_path()?;
    if path.exists() && !yes && !confirm_replace(&path)? {
        println!("Setup cancelled; kept {}.", path.display());
        return Ok(());
    }
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    let config = merge(existing.as_deref(), &server_url, peer.as_deref())
        .with_context(|| format!("cannot update {}", path.display()))?;
    write_config(&path, &config)?;
    println!("Wrote {}.", path.display());
    println!("Start mesh-daemon to connect to the mesh.");
    Ok(())
}

/// Set the server and the pinned server identity in `existing`, keeping
/// every other setting. `peer` is a login or an ACL tag; without one, the
/// pinned logins and tags are removed.
fn merge(existing: Option<&str>, server_url: &str, peer: Option<&str>) -> Result<String> {
    let mut config: Table = match existing {
        Some(text) => text
            .parse()
            .context("the existing config is not valid TOML")?,
        None => Table::new(),
    };
    config.insert(
        "server_url".to_owned(),
        Value::String(server_url.to_owned()),
    );
    let mut peers = match config.remove("allowed_peers") {
        Some(Value::Table(table)) => table,
        Some(_) => bail!("allowed_peers must be a table"),
        None => Table::new(),
    };
    peers.remove("logins");
    peers.remove("tags");
    if let Some(peer) = peer {
        let key = if peer.starts_with("tag:") {
            "tags"
        } else {
            "logins"
        };
        peers.insert(
            key.to_owned(),
            Value::Array(vec![Value::String(peer.to_owned())]),
        );
    }
    if !peers.is_empty() {
        config.insert("allowed_peers".to_owned(), Value::Table(peers));
    }
    toml::to_string(&config).context("cannot serialize daemon config")
}

fn validate_url(input: &str) -> Result<String> {
    let url = Url::parse(input.trim()).context("invalid mesh server URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("use an http:// or https:// server origin, such as http://mesh-host:7070");
    }
    Ok(url.origin().ascii_serialization())
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("cannot find the user config directory")?;
    Ok(dir.join("mesh").join("daemon.toml"))
}

fn prompt(message: &str) -> Result<String> {
    eprint!("{message}");
    io::stderr().flush().context("cannot write setup prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("cannot read setup answer")?;
    if answer.trim().is_empty() {
        bail!("a value is required");
    }
    Ok(answer.trim().to_owned())
}

fn prompt_optional(message: &str) -> Result<Option<String>> {
    eprint!("{message}");
    io::stderr().flush().context("cannot write setup prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("cannot read setup answer")?;
    let answer = answer.trim();
    Ok((!answer.is_empty()).then(|| answer.to_owned()))
}

fn confirm_replace(path: &Path) -> Result<bool> {
    eprint!("{} already exists. Replace it? [y/N] ", path.display());
    io::stderr().flush().context("cannot write setup prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("cannot read setup answer")?;
    Ok(answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"))
}

fn write_config(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid config path"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!(
            "refusing to replace non-regular config path {}",
            path.display()
        );
    }
    let mut reserved = None;
    for attempt in 0..100 {
        let mut name = path
            .file_name()
            .ok_or_else(|| anyhow!("invalid config path"))?
            .to_os_string();
        name.push(format!(".meshctl-{}-{attempt}.tmp", std::process::id()));
        let candidate = parent.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                reserved = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot create config beside {}", path.display()));
            }
        }
    }
    let (temp_path, mut file) = reserved.ok_or_else(|| {
        anyhow!(
            "cannot reserve a temporary config file beside {}",
            path.display()
        )
    })?;
    let write_result = (|| -> Result<()> {
        file.write_all(contents.as_bytes())
            .with_context(|| format!("cannot write temporary config for {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot sync temporary config for {}", path.display()))?;
        drop(file);
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("cannot replace config {}", path.display()))
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_url_must_be_an_http_origin() {
        assert_eq!(
            validate_url("https://mesh.graybird.software/").unwrap(),
            "https://mesh.graybird.software"
        );
        assert!(validate_url("https://mesh.graybird.software/mesh").is_err());
        assert!(validate_url("https://user@mesh.graybird.software").is_err());
    }

    #[test]
    fn setup_keeps_unrelated_settings() {
        let existing = "server_url = \"http://old:7070\"\nnode = \"box\"\ndata_dir = \"/srv/mesh\"\n\n[allowed_peers]\nlogins = [\"old@example.com\"]\nallow_loopback = true\n\n[reserve]\nfive_hour = 0.2\n";
        let merged: Table = merge(Some(existing), "https://mesh.example", Some("tag:mesh"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(merged["server_url"].as_str(), Some("https://mesh.example"));
        assert_eq!(merged["node"].as_str(), Some("box"));
        assert_eq!(merged["data_dir"].as_str(), Some("/srv/mesh"));
        assert_eq!(merged["reserve"]["five_hour"].as_float(), Some(0.2));
        let peers = merged["allowed_peers"].as_table().unwrap();
        assert!(!peers.contains_key("logins"));
        assert_eq!(peers["tags"][0].as_str(), Some("tag:mesh"));
        assert_eq!(peers["allow_loopback"].as_bool(), Some(true));
    }

    #[test]
    fn setup_without_a_peer_clears_the_pinned_identity() {
        let existing = "server_url = \"http://old:7070\"\n[allowed_peers]\nlogins = [\"a@b\"]\n";
        let merged = merge(Some(existing), "https://mesh.example", None).unwrap();
        assert_eq!(merged, "server_url = \"https://mesh.example\"\n");
        assert!(merge(Some("not = [valid"), "https://mesh.example", None).is_err());
    }

    #[test]
    fn config_write_replaces_a_regular_file_with_complete_contents() {
        let dir = std::env::temp_dir().join(format!("meshctl-setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.toml");
        std::fs::write(&path, "old config\n").unwrap();

        write_config(&path, "server_url = \"https://mesh.example\"\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "server_url = \"https://mesh.example\"\n"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "temporary config file is removed after replacement"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
