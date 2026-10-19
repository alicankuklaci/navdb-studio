#!/usr/bin/env bash
# Build "NavDB Studio.app" from a compiled navdb binary.
#
#   scripts/bundle-macos.sh <path/to/navdb> <out-dir> [version]
#
# Produces <out-dir>/NavDB Studio.app (ad-hoc signed). The icon is generated
# from crates/navdb-app/assets/icon/navdb.png with sips + iconutil.
set -euo pipefail

bin="$1"
out="$2"
root="$(cd "$(dirname "$0")/.." && pwd)"
version="${3:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)}"

app="$out/NavDB Studio.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$bin" "$app/Contents/MacOS/navdb"
chmod +x "$app/Contents/MacOS/navdb"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"

iconset="$(mktemp -d)/navdb.iconset"
mkdir -p "$iconset"
src="$root/crates/navdb-app/assets/icon/navdb.png"
for s in 16 32 128 256 512; do
  sips -z "$s" "$s" "$src" --out "$iconset/icon_${s}x${s}.png" >/dev/null
  sips -z "$((s * 2))" "$((s * 2))" "$src" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/navdb.icns"
rm -rf "$(dirname "$iconset")"

# ad-hoc signature: required for arm64 and keeps the bundle seal consistent
codesign --force --deep --sign - "$app"
echo "$app"
