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
following the `[consumers]` or `[daemons]` logins as whois would; a consumer
shows as node `via-proxy`. Daemons can connect through the proxy too: a
daemon gets exactly the rights its login would have by whois, and its node
name (from `node` in `daemon.toml`, by default its host name) is only a
label under that login, so a node name that a daemon of another login holds
is refused. ACL tags are unknown through the proxy, so `tags` never match
there. A request from a proxy address without exactly one plausible login is
refused, as are browser requests that another site made. From other
addresses the header is ignored, and it is never forwarded to daemons. The
proxy must keep the request's `Host`.

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

The daemon tunnel is an HTTP/1.1 `Upgrade` (`mesh-tunnel/1`), which Caddy's
`reverse_proxy` passes through as it does WebSockets; reloading Caddy's
config closes it, and the daemon dials again. Point daemons at the proxy
with an `https://` URL:

```toml
server_url = "https://mesh.example.com"
```

Over `https://` the daemon checks the server's certificate against the
public web PKI roots, offers only HTTP/1.1, and skips the whois check of
`[allowed_peers]`, which applies to `http://` servers only. The daemon's
machine must reach the proxy over the tailnet, so that `tailscale_auth`
can identify it.

**A server without Tailscale.** When every caller comes through the proxy,
the server host needs no tailscaled:

```toml
listen = "10.10.10.102:7070"
[trusted_proxy]
addrs = ["10.10.10.111"]
only = true
[consumers]
logins = ["you@example.com", "friend@example.com"]
[daemons]
logins = ["you@example.com", "friend@example.com"]
```

With `only`, whois is never asked and other addresses are refused (loopback
too, unless `allow_loopback` is set). The server cannot learn its own login
without tailscaled, so `logins` must be listed for both tables.

**Warning:** anyone who can connect from a trusted address can act as any
user. Use an address that only the proxy can use, make the proxy replace any
`X-Webauth-User` header a client sends, and do not list a shared address such
as `127.0.0.1` on a machine where other users or programs run. A listed
address is never checked with whois, even when `allow_loopback` is on. With
`only`, bind `listen` to an address that only the proxy can reach, or
firewall it.

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
