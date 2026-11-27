#!/bin/sh
# Packs built files into one release archive. The release workflow runs this,
# and you can run it by hand to test an archive.
#
#   scripts/package.sh VERSION PLATFORM BIN_DIR OTHER_BIN_DIR WEB_DIR OUT_DIR
#
# PLATFORM is x86_64-linux or x86_64-windows. BIN_DIR holds the built
# binaries. WEB_DIR is the built dashboard (trunk's dist); the Windows archive
# does not use it. The archive is OUT_DIR/mesh-VERSION-PLATFORM.tar.gz or .zip,
# with every file under one directory, mesh-VERSION-PLATFORM/.

set -eu
umask 022
export TZ=UTC

[ $# -eq 6 ] || {
    echo 'Usage: package.sh VERSION PLATFORM BIN_DIR OTHER_BIN_DIR WEB_DIR OUT_DIR' >&2
    exit 2
}

version=$1
platform=$2
bin_dir=$3
other_bin_dir=$4
web_dir=$5
out_dir=$6

printf '%s\n' "$version" | LC_ALL=C awk '
    NR == 1 && /^[0-9]+\.[0-9]+\.[0-9]+$/ { ok = 1 }
    END { exit !(NR == 1 && ok) }
' || { echo 'Expected version X.Y.Z' >&2; exit 2; }
epoch=${SOURCE_DATE_EPOCH:-315532800}

case $platform in
x86_64-linux | x86_64-windows) ;;
*)
    echo "error: unknown platform $platform (use x86_64-linux or x86_64-windows)" >&2
    exit 2
    ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
name=mesh-$version-$platform

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM
stage=$work/$name
mkdir -p -- "$stage" "$out_dir"
out_dir=$(cd "$out_dir" && pwd)

put() {
    install -D -m "$1" "$2" "$stage/$3"
}

case $platform in
x86_64-linux)
    for bin in mesh-server mesh-daemon meshctl; do
        put 0755 "$bin_dir/$bin" "$bin"
    done
    [ -f "$web_dir/index.html" ] || {
        echo "error: $web_dir has no index.html (build it with trunk)" >&2
        exit 1
    }
    mkdir "$stage/web"
    special=$(find "$web_dir/." ! -type f ! -type d -print -quit)
    [ -z "$special" ] || { echo "Unsupported dashboard file: $special" >&2; exit 1; }
    cp -R -- "$web_dir"/. "$stage/web/"
    rm -f "$stage/web/.trunk-lock"
    mkdir -p "$stage/web/downloads"
    put 0755 "$bin_dir/meshctl" web/downloads/meshctl-linux-x86_64
    put 0755 "$bin_dir/mesh-daemon" web/downloads/mesh-daemon-linux-x86_64
    put 0644 "$other_bin_dir/meshctl.exe" web/downloads/meshctl-windows-x86_64.exe
    put 0644 "$other_bin_dir/mesh-daemon.exe" web/downloads/mesh-daemon-windows-x86_64.exe
    put 0644 "$root/crates/mesh-web/install.html" web/install.html
    chmod -R u=rwX,go=rX "$stage/web"
    put 0644 "$root/contrib/mesh-server.service" contrib/mesh-server.service
    put 0644 "$root/contrib/mesh-daemon.service" contrib/mesh-daemon.service
    (
        cd "$work"
        tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
            -czf "$out_dir/$name.tar.gz" "$name"
    )
    echo "$out_dir/$name.tar.gz"
    ;;
x86_64-windows)
    put 0755 "$bin_dir/mesh-daemon.exe" mesh-daemon.exe
    put 0755 "$bin_dir/meshctl.exe" meshctl.exe
    put 0644 "$root/contrib/mesh-daemon.ps1" mesh-daemon.ps1
    find "$stage" -exec touch -d "@$epoch" {} +
    rm -f "$out_dir/$name.zip"
    (
        cd "$work"
        zip -qrX "$out_dir/$name.zip" "$name"
    )
    echo "$out_dir/$name.zip"
    ;;
esac
