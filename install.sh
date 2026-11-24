#!/bin/sh
# Installs mesh on Linux (x86_64).
#
#   curl -fsSL https://raw.githubusercontent.com/Calastrophe/sub-mesh/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/Calastrophe/sub-mesh/main/install.sh | sh -s -- --server
#
# The first line installs mesh-daemon and meshctl for your user. The second
# installs mesh-server for the whole machine. Run with --help for the options.

set -eu

REPO="${MESH_REPO:-Calastrophe/sub-mesh}"

PLATFORM=x86_64-linux
MODE=daemon
ACTION=install
SERVICE=yes
ASSUME_YES=no
SERVER_URL="${MESH_SERVER_URL:-}"
DESTDIR="${MESH_DESTDIR:-}"
URL_PLACEHOLDER='http://<server>:7070'

tmp=
USE_SUDO=no

as_root() {
    if [ "$USE_SUDO" = yes ]; then
        if [ -t 0 ] || ( : </dev/tty ) 2>/dev/null; then
            sudo "$@"
        else
            sudo -n "$@"
        fi
    else
        "$@"
    fi
}

say() { printf '%s\n' "$*"; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<EOF
Usage: install.sh [options]

Installs mesh-daemon and meshctl for your user, in ~/.local/bin. It needs no
root. With --server it installs mesh-server for the whole machine instead.

Options:
  --server           install mesh-server (uses sudo)
  --server-url URL   address of mesh-server for the daemon, $URL_PLACEHOLDER
  --no-service       do not install or start the systemd unit
  --uninstall        remove what this script installed (config and data stay)
  -y, --yes          do not ask before using sudo
  -h, --help         show this help

Environment:
  MESH_VERSION       install this version, such as 0.1.0 (default: the latest)
  MESH_SERVER_URL    the same as --server-url
  MESH_REPO          GitHub repository to download from (default: $REPO)
  MESH_DOWNLOAD_URL  download from this directory URL instead of GitHub
  MESH_DESTDIR       with --server: install under this directory, without
                     sudo, a system user, or systemctl (for testing)
EOF
}

cleanup() {
    [ -z "$tmp" ] || rm -rf "$tmp"
}

parse_args() {
    while [ $# -gt 0 ]; do
        case $1 in
        --server) MODE=server ;;
        --server-url)
            [ $# -ge 2 ] || die "--server-url needs a value"
            SERVER_URL=$2
            shift
            ;;
        --server-url=*) SERVER_URL=${1#*=} ;;
        --no-service) SERVICE=no ;;
        --uninstall) ACTION=uninstall ;;
        -y | --yes) ASSUME_YES=yes ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown option: $1 (try --help)" ;;
        esac
        shift
    done
}

check_platform() {
    [ "$(uname -s)" = Linux ] || die "this script is for Linux; on Windows use install.ps1"
    case $(uname -m) in
    x86_64 | amd64) ;;
    *) die "mesh publishes x86_64 builds only (this machine is $(uname -m))" ;;
    esac
}

check_server_url() {
    [ -n "$SERVER_URL" ] || return 0
    printf '%s\n' "$SERVER_URL" | LC_ALL=C awk '
        NR == 1 && /^https?:\/\/[^[:space:][:cntrl:]"\\]+$/ { ok = 1 }
        END { exit !(NR == 1 && ok) }
    ' || die "invalid server URL (use http://HOST:7070 or https://HOST)"
}

set_paths() {
    if [ "$MODE" = server ]; then
        if [ -n "$DESTDIR" ]; then
            case $DESTDIR in /*) ;; *) die "MESH_DESTDIR must be absolute" ;; esac
            DESTDIR=$(realpath -m "$DESTDIR")
            [ "$DESTDIR" != / ] || die "MESH_DESTDIR must not be /"
        fi
        BIN_DIR=$DESTDIR/usr/local/bin
        WEB_DIR=$DESTDIR/usr/share/mesh/web
        CONF_DIR=$DESTDIR/etc/mesh
        UNIT_DIR=$DESTDIR/etc/systemd/system
        UNIT_KEY=unit
        STATE=$DESTDIR/usr/share/mesh/install
        if [ -z "$DESTDIR" ] && [ "$(id -u)" -ne 0 ]; then
            command -v sudo >/dev/null 2>&1 || die "sudo is required (or run this script as root)"
            USE_SUDO=yes
        fi
    else
        case ${HOME:-} in /*) ;; *) die "HOME must be absolute" ;; esac
        config_home=${XDG_CONFIG_HOME:-$HOME/.config}
        case $config_home in /*) ;; *) config_home=$HOME/.config ;; esac
        BIN_DIR=$HOME/.local/bin
        CONF_DIR=$config_home/mesh
        UNIT_DIR=$config_home/systemd/user
        unit_hash=$(printf '%s' "$UNIT_DIR" | sha256sum)
        UNIT_KEY=unit-${unit_hash%% *}
        STATE=$HOME/.local/share/mesh/install
    fi
}

ask_sudo() {
    [ "$USE_SUDO" = yes ] || return 0
    printf '%s\n' "$1"
    [ "$ASSUME_YES" = no ] || return 0
    ( : </dev/tty ) 2>/dev/null || die "cannot ask without a terminal; run again with --yes and arrange sudo access first"
    printf 'Continue? [y/N] ' >/dev/tty
    read -r answer </dev/tty
    case $answer in
    y | Y | yes | YES) ;;
    *) die "cancelled" ;;
    esac
}

fetch() {
    curl -q --globoff --proto "$protocols" --proto-redir "$protocols" \
        -fsSL --retry 3 -o "$2" -- "$1" || die "download failed: $1"
}

download() {
    repo_url=https://github.com/$REPO/releases
    BASE_URL=${MESH_DOWNLOAD_URL:-}
    BASE_URL=${BASE_URL%/}
    VERSION=${MESH_VERSION:-}
    VERSION=${VERSION#v}
    protocols='=https'
    case $BASE_URL in
        http://?*) protocols='=http,https' ;;
        https://?* | '') ;;
        *) die "invalid MESH_DOWNLOAD_URL" ;;
    esac
    if [ -n "$BASE_URL" ]; then
        fetch "$BASE_URL/SHA256SUMS" "$tmp/sums"
        if [ -z "$VERSION" ]; then
            VERSION=$(awk '
                length($1) == 64 && $1 !~ /[^[:xdigit:]]/ &&
                $2 ~ /^\*?mesh-[0-9]+\.[0-9]+\.[0-9]+-x86_64-linux\.tar\.gz$/ {
                    v = $2; sub(/^\*?mesh-/, "", v); sub(/-x86_64-linux\.tar\.gz$/, "", v)
                    print v
                }' "$tmp/sums")
        fi
    else
        printf '%s\n' "$REPO" | LC_ALL=C awk '
            NR == 1 && /^[A-Za-z0-9_-]+\/[A-Za-z0-9_.-]+$/ { ok = 1 }
            END { exit !(NR == 1 && ok) }
        ' || die "invalid MESH_REPO"
        if [ -z "$VERSION" ]; then
            url=$(curl -q --proto '=https' --proto-redir '=https' -fsSL -o /dev/null \
                -w '%{url_effective}' "$repo_url/latest") || die "cannot find the latest release"
            case $url in
                "$repo_url"/tag/v*) VERSION=${url#"$repo_url"/tag/v} ;;
                *) die "unexpected latest release URL: $url" ;;
            esac
        fi
        BASE_URL=$repo_url/download/v$VERSION
    fi
    printf '%s\n' "$VERSION" | LC_ALL=C awk '
        NR == 1 && /^[0-9]+\.[0-9]+\.[0-9]+$/ { ok = 1 }
        END { exit !(NR == 1 && ok) }
    ' || die "expected exactly one release version in X.Y.Z format"

    ASSET=mesh-$VERSION-$PLATFORM.tar.gz
    [ -f "$tmp/sums" ] || fetch "$BASE_URL/SHA256SUMS" "$tmp/sums"
    expected=$(LC_ALL=C awk -v asset="$ASSET" '
        { file = $2; sub(/^\*/, "", file) }
        file == asset { count++; hash = $1; fields = NF }
        END {
            if (count != 1 || fields != 2 || length(hash) != 64 || hash ~ /[^[:xdigit:]]/) exit 1
            print tolower(hash)
        }' "$tmp/sums") || die "expected exactly one valid SHA256SUMS entry for $ASSET"
    fetch "$BASE_URL/$ASSET" "$tmp/$ASSET"
    (cd "$tmp" && printf '%s  %s\n' "$expected" "$ASSET" | sha256sum -c --status) ||
        die "checksum mismatch for $ASSET"

    PKG=$tmp/pkg
    mkdir "$PKG"
    tar -xzf "$tmp/$ASSET" -C "$PKG" --strip-components=1 --no-same-owner \
        --no-same-permissions "mesh-$VERSION-$PLATFORM"
    special=$(find "$PKG" ! -type f ! -type d -print -quit)
    [ -z "$special" ] || die "archive contains a special file or symlink"
}

check_path() {
    if [ -f "$STATE/$1" ]; then
        return
    fi
    if [ -e "$2" ] || [ -L "$2" ]; then die "existing path is outside this install: $2"; fi
}

install_file() {
    as_root mkdir -p "$(dirname "$3")"
    staged=$(as_root mktemp "$3.XXXXXX")
    if ! as_root install -m "$1" "$2" "$staged" ||
        { [ "$#" -eq 4 ] && ! as_root touch "$STATE/$4"; } ||
        ! as_root mv -fT "$staged" "$3"; then
        as_root rm -f "$staged"
        die "could not install $3"
    fi
}

remove() {
    [ -f "$STATE/$1" ] || return 0
    as_root rm -f "$2"
    as_root rm -f "$STATE/$1"
}

have_systemd() {
    [ "$SERVICE" = yes ] || return 1
    command -v systemctl >/dev/null 2>&1 || return 1
    if [ "$MODE" = server ]; then
        [ -z "$DESTDIR" ] && [ -d /run/systemd/system ]
    else
        systemctl --user show-environment >/dev/null 2>&1
    fi
}

ctl() {
    if [ "$MODE" = server ]; then
        as_root systemctl "$@"
    else
        systemctl --user "$@"
    fi
}

write_daemon_config() {
    conf=$CONF_DIR/daemon.toml
    if [ ! -e "$conf" ] && [ ! -L "$conf" ]; then
        mkdir -p "$CONF_DIR"
        (umask 077; set -C; printf 'server_url = "%s"\n' "${SERVER_URL:-$URL_PLACEHOLDER}" >"$conf")
    fi
    CONFIGURED=no
    if [ -r "$conf" ] && ! grep -qF "\"$URL_PLACEHOLDER\"" "$conf"; then
        CONFIGURED=yes
    fi
}

install_daemon() {
    check_path daemon "$BIN_DIR/mesh-daemon"
    check_path ctl "$BIN_DIR/meshctl"
    if have_systemd; then
        check_path "$UNIT_KEY" "$UNIT_DIR/mesh-daemon.service"
        install_file 0644 "$PKG/contrib/mesh-daemon.service" "$UNIT_DIR/mesh-daemon.service" "$UNIT_KEY"
    fi
    install_file 0755 "$PKG/mesh-daemon" "$BIN_DIR/mesh-daemon" daemon
    install_file 0755 "$PKG/meshctl" "$BIN_DIR/meshctl" ctl
    write_daemon_config

    if have_systemd; then
        ctl daemon-reload
        if [ "$CONFIGURED" = yes ]; then
            ctl enable mesh-daemon
            ctl restart mesh-daemon
        fi
    fi
    case ":${PATH:-}:" in
        *":$BIN_DIR:"*) ;;
        *) say "Add $BIN_DIR to your PATH." ;;
    esac
    if [ "$CONFIGURED" = no ]; then
        say "Set server_url in $CONF_DIR/daemon.toml."
        if have_systemd; then
            say 'Then run systemctl --user enable --now mesh-daemon.'
        fi
    fi
    if ! have_systemd; then
        say 'Start mesh-daemon from your shell.'
    fi
}

web_paths() {
    # Use a predictable record format and reject names that would break it.
    bad=$(LC_ALL=C find "$1" -mindepth 1 -name '*[!A-Za-z0-9_.-]*' -print -quit)
    [ -z "$bad" ] || die "unsupported dashboard filename in $1"
    find "$1" -mindepth 1 -printf '%y %P\0' >"$tmp/paths"
    LC_ALL=C sort -z "$tmp/paths" | tr '\000' '\n' | LC_ALL=C awk '
        /^[df] [A-Za-z0-9_.\/-]+$/ { print; next }
        { exit 1 }
    ' || die "unsupported dashboard path in $1"
}

remove_web() {
    [ -f "$STATE/web.paths" ] || return 0
    if [ -d "$WEB_DIR" ] && [ ! -L "$WEB_DIR" ]; then
        # Process children before their parent directories.
        LC_ALL=C sort -r -k2 "$STATE/web.paths" | while read -r type path; do
            target=$WEB_DIR/$path
            parent=$(dirname "$target")
            [ "$(realpath -e "$parent" 2>/dev/null)" = "$parent" ] || continue
            case $type in
                f) as_root rm -f "$target" ;;
                d) as_root rmdir "$target" 2>/dev/null || : ;;
            esac
        done
        as_root rmdir "$WEB_DIR" 2>/dev/null || :
    fi
    as_root rm -f "$STATE/web.paths"
    as_root rm -f "$STATE/web"
}

install_server() {
    check_path server "$BIN_DIR/mesh-server"
    check_path web "$WEB_DIR"
    if have_systemd; then
        check_path "$UNIT_KEY" "$UNIT_DIR/mesh-server.service"
    fi
    if [ -z "$DESTDIR" ] && ! id mesh >/dev/null 2>&1; then
        as_root useradd --system --home-dir /var/lib/mesh --shell /usr/sbin/nologin mesh
    fi

    web_paths "$PKG/web" >"$tmp/web.paths"
    if [ -d "$WEB_DIR" ] && [ ! -L "$WEB_DIR" ]; then
        web_paths "$WEB_DIR" >"$tmp/current.paths"
        [ -f "$STATE/web.paths" ] || die "missing dashboard install record"
        extra=$(LC_ALL=C comm -23 "$tmp/current.paths" "$STATE/web.paths")
        [ -z "$extra" ] || die "the dashboard directory contains files outside this install"
    fi
    as_root mkdir -p "$(dirname "$WEB_DIR")"
    web=$(as_root mktemp -d "$WEB_DIR.XXXXXX")
    if ! as_root cp -R "$PKG/web/." "$web" || ! as_root chmod -R u=rwX,go=rX "$web" ||
        ! as_root touch "$STATE/web" || ! as_root rm -rf "$WEB_DIR" || ! as_root mv -T "$web" "$WEB_DIR"; then
        as_root rm -rf "$web"
        die "could not install dashboard"
    fi
    install_file 0644 "$tmp/web.paths" "$STATE/web.paths"
    install_file 0755 "$PKG/mesh-server" "$BIN_DIR/mesh-server" server

    conf=$CONF_DIR/server.toml
    if [ ! -e "$conf" ] && [ ! -L "$conf" ]; then
        as_root mkdir -p "$CONF_DIR"
        staged=$(as_root mktemp "$conf.XXXXXX")
        if [ -z "$DESTDIR" ]; then
            as_root chgrp "$(id -g mesh)" "$staged"
        fi
        as_root chmod 0640 "$staged"
        as_root ln -T "$staged" "$conf" || {
            as_root rm -f "$staged"
            die "could not create $conf"
        }
        as_root rm -f "$staged"
    fi

    if have_systemd; then
        install_file 0644 "$PKG/contrib/mesh-server.service" "$UNIT_DIR/mesh-server.service" "$UNIT_KEY"
        ctl daemon-reload
        ctl enable mesh-server
        ctl restart mesh-server
    fi
    say 'Give mesh-server access to tailscaled with sudo tailscale set --operator=mesh.'
}

uninstall() {
    name=mesh-$MODE
    SERVICE=yes
    if [ -f "$STATE/$UNIT_KEY" ] && have_systemd; then
        if [ -f "$UNIT_DIR/$name.service" ]; then
            ctl disable --now "$name"
        fi
    fi
    remove "$UNIT_KEY" "$UNIT_DIR/$name.service"
    if have_systemd; then ctl daemon-reload; fi
    remove "$MODE" "$BIN_DIR/$name"
    if [ "$MODE" = daemon ]; then
        remove ctl "$BIN_DIR/meshctl"
    elif [ -f "$STATE/web" ]; then
        remove_web
    fi
    say 'Kept mesh config and data.'
}

parse_args "$@"
check_platform
set_paths
if [ "$ACTION" = uninstall ]; then
    [ -d "$STATE" ] || exit 0
    ask_sudo "Remove the installer-managed mesh-server files and service."
    exec 9<"$STATE"
    flock -x 9
    uninstall
    exit
fi

check_server_url
tmp=$(mktemp -d)
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
download
if [ "$MODE" = server ]; then
    paths='mesh-server web/index.html contrib/mesh-server.service'
else
    paths='mesh-daemon meshctl contrib/mesh-daemon.service'
fi
for path in $paths; do
    [ -f "$PKG/$path" ] || die "missing $path in $ASSET"
done

ask_sudo "Install mesh-server and the dashboard system-wide and configure the service unless skipped."
as_root mkdir -p "$STATE"
exec 9<"$STATE"
flock -x 9
if [ "$MODE" = server ]; then install_server; else install_daemon; fi
say "mesh $VERSION is installed."
