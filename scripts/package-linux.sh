#!/usr/bin/env bash
# Package a Linux navdb binary as a .tar.gz and a .deb.
#
#   scripts/package-linux.sh <path/to/navdb> <out-dir> <version> [deb-arch]
#
# Produces <out-dir>/navdb-studio-<version>-linux-x86_64.tar.gz and
# <out-dir>/navdb-studio_<version>_<deb-arch>.deb (deb-arch defaults to amd64).
set -euo pipefail

bin="$1"
out="$2"
version="$3"
arch="${4:-amd64}"
root="$(cd "$(dirname "$0")/.." && pwd)"
case "$arch" in amd64) tarch=x86_64 ;; arm64) tarch=aarch64 ;; *) tarch="$arch" ;; esac
mkdir -p "$out"
work="$(mktemp -d)"

# --- tarball: binary + desktop entry + icon
name="navdb-studio-$version-linux-$tarch"
mkdir -p "$work/$name"
install -m755 "$bin" "$work/$name/navdb"
cp "$root/packaging/linux/navdb-studio.desktop" "$work/$name/"
cp "$root/crates/navdb-app/assets/icon/navdb-256.png" "$work/$name/navdb-studio.png"
cp "$root/README.md" "$work/$name/"
tar -C "$work" -czf "$out/$name.tar.gz" "$name"

# --- .deb
deb="$work/deb"
install -Dm755 "$bin" "$deb/usr/bin/navdb"
install -Dm644 "$root/packaging/linux/navdb-studio.desktop" "$deb/usr/share/applications/navdb-studio.desktop"
install -Dm644 "$root/crates/navdb-app/assets/icon/navdb-256.png" "$deb/usr/share/icons/hicolor/256x256/apps/navdb-studio.png"
mkdir -p "$deb/DEBIAN"
cat > "$deb/DEBIAN/control" <<EOF
Package: navdb-studio
Version: ${version#v}
Section: database
Priority: optional
Architecture: $arch
Depends: libc6, libssl3, libgl1, libxkbcommon0
Recommends: libegl1, libx11-6, libxcursor1, libxi6, libxrandr2, libwayland-client0, libxkbcommon-x11-0, xdg-desktop-portal, openssh-client
Maintainer: NavDB Studio <noreply@navdb.invalid>
Description: Multi-database administration GUI
 MySQL, MariaDB, PostgreSQL, SQLite, Redis, MongoDB, SQL Server, Oracle,
 DuckDB and more. Oracle connections need Oracle Instant Client at runtime.
EOF
dpkg-deb --root-owner-group --build "$deb" "$out/navdb-studio_${version#v}_$arch.deb"

rm -rf "$work"
ls -l "$out"
