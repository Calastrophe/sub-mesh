# sub-mesh

Pool Claude and Codex subscriptions across machines over Tailnet. Run
`mesh-server` on an always-on Linux host and `mesh-daemon` on each machine
with subscriptions. Use `meshctl` to manage the local daemon.

## Install

Releases support x86_64 Linux and Windows. Replace `mesh-host` with your
server's Tailscale name or IP address in the daemon and client commands.

**Server (Linux)**

```sh
curl -fsSL https://raw.githubusercontent.com/Calastrophe/sub-mesh/main/install.sh | sh -s -- --server
sudo tailscale set --operator=mesh
```

The installer uses `sudo` to install the systemd service. The dashboard is
at `http://mesh-host:7070/`. By default the server accepts peers owned by
its own tailnet login.

**Daemon (Linux)**

```sh
curl -fsSL https://raw.githubusercontent.com/Calastrophe/sub-mesh/main/install.sh | sh -s -- --server-url http://mesh-host:7070
```

**Daemon (Windows PowerShell)**

```powershell
$env:MESH_SERVER_URL = 'http://mesh-host:7070'
irm https://raw.githubusercontent.com/Calastrophe/sub-mesh/main/install.ps1 | iex
```

Run `meshctl login codex` or `meshctl login claude`, then check
`meshctl status`. Run the installer again to upgrade.

## Configuration

By default the server reads `/etc/mesh/server.toml`, and the daemon reads
`~/.config/mesh/daemon.toml` on Linux (honoring `XDG_CONFIG_HOME`) and
`%APPDATA%\mesh\daemon.toml` on Windows. Restart the service after a
change.

**Server**

```toml
[consumers]              # who may use the API and dashboard
logins = ["you@example.com"]

[daemons]                # who may connect daemons
logins = ["you@example.com"]
tags = ["tag:mesh"]
```

Empty `logins` means the server's own tailnet login.

**Daemon**

```toml
server_url = "http://mesh-host:7070"

[allowed_peers]          # which server to trust, http:// only
logins = ["you@example.com"]
```

**Reverse proxy**

To run the server behind a proxy that authenticates tailnet users, such as
Caddy with `tailscale_auth`, add this to `server.toml`:

```toml
[trusted_proxy]
addrs = ["10.0.0.5"]      # the address the proxy connects from
header = "x-webauth-user" # the login header the proxy sets
only = true               # refuse direct callers; no tailscaled needed
```

The server trusts the header from these addresses alone, so make sure only
the proxy can reach the server. With `only`, set explicit `logins` in
`[consumers]` and `[daemons]`. Point
daemons at the proxy with an `https://` `server_url` and leave
`[allowed_peers]` empty.

## Clients

**Claude Code**

```sh
export ANTHROPIC_BASE_URL=http://mesh-host:7070/claude
export ANTHROPIC_AUTH_TOKEN=mesh
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

**Codex CLI**

Set `MESH_API_KEY` to any value and add this to `~/.codex/config.toml`.

```toml
model_provider = "mesh"

[model_providers.mesh]
name = "mesh"
base_url = "http://mesh-host:7070/codex"
model_catalog_url = "http://mesh-host:7070/codex/models"
wire_api = "responses"
env_key = "MESH_API_KEY"
supports_websockets = true
```
