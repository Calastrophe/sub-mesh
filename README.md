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

## Behind a reverse proxy

The server identifies callers by asking tailscaled who owns the connecting
tailnet address. A reverse proxy that identifies tailnet users itself can
name the caller instead. Add the proxy to `/etc/mesh/server.toml`:

```toml
[trusted_proxy]
addrs = ["10.10.10.111"]    # the address the proxy connects from
header = "x-webauth-user"   # the default
```

A request from one of `addrs` is admitted by the login in `header`,
following the `[consumers]` logins as whois would; its node shows as
`via-proxy`. A request from a proxy address without exactly one plausible
login is refused, as are browser requests that another site made. Daemon
routes are refused through the proxy: daemons connect to the server directly
over the tailnet. From other addresses the header is ignored, and it is
never forwarded to daemons. The proxy must keep the request's `Host`.

With Caddy and the `tailscale_auth` plugin (`route` keeps the directives in
this order, so the header is set after authentication and replaces any value
the client sent):

```caddy
mesh.example.com {
	route {
		tailscale_auth
		request_header X-Webauth-User {http.auth.user.tailscale_user}
		reverse_proxy mesh-host:7070
	}
}
```

**Warning:** anyone who can connect from a trusted address can act as any
user. Use an address that only the proxy can use, make the proxy replace any
`X-Webauth-User` header a client sends, and do not list a shared address such
as `127.0.0.1` on a machine where other users or programs run. A listed
address is never checked with whois, even when `allow_loopback` is on.

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
