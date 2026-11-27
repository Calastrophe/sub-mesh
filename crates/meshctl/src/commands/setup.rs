use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use url::Url;

#[derive(Serialize)]
struct DaemonConfig {
    server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_peers: Option<AllowedPeers>,
}

#[derive(Serialize)]
struct AllowedPeers {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    logins: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
}

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

    let peers = peer.map(|peer| {
        if peer.starts_with("tag:") {
            AllowedPeers {
                logins: Vec::new(),
                tags: vec![peer],
            }
        } else {
            AllowedPeers {
                logins: vec![peer],
                tags: Vec::new(),
            }
        }
    });
    let config = toml::to_string(&DaemonConfig {
        server_url,
        allowed_peers: peers,
    })
    .context("cannot serialize daemon config")?;

    let path = config_path()?;
    if path.exists() && !yes && !confirm_replace(&path)? {
        println!("Setup cancelled; kept {}.", path.display());
        return Ok(());
    }
    write_config(&path, &config)?;
    println!("Wrote {}.", path.display());
    println!("Start mesh-daemon to connect to the mesh.");
    Ok(())
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
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "refusing to replace non-regular config path {}",
                path.display()
            );
        }
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
