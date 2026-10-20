#!/usr/bin/env bash
# Build, sign and package NavDB Studio for the Mac App Store.
#
#   scripts/build-appstore.sh [--upload] [out-dir]
#
# Produces <out-dir>/NavDB-Studio-appstore.pkg (default out-dir: dist/appstore):
# a universal (arm64 + x86_64) sandboxed build with the StoreKit subscription,
# without Oracle and SSH tunnels (they cannot work inside the App Sandbox).
#
# Needs, in the login keychain:
#   "Apple Distribution: … (TEAM_ID)"           signs the app
#   "3rd Party Mac Developer Installer: … (TEAM_ID)" or
#   "Mac Installer Distribution: … (TEAM_ID)"   signs the installer package
# and a "Mac App Store Connect" provisioning profile for dev.navdb.studio.
#
# Environment:
#   TEAM_ID        default PA45BX35VE
#   PROFILE        path to the .provisionprofile (default packaging/macos/appstore.provisionprofile)
#   BUILD_NUMBER   CFBundleVersion, must grow with every upload (default: UTC timestamp)
#   ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_FILE
#                  App Store Connect API key for --upload (ASC_KEY_FILE is the
#                  AuthKey_<ASC_KEY_ID>.p8 path, default ~/.config/appstoreconnect/)
set -euo pipefail

upload=0
if [[ "${1:-}" == "--upload" ]]; then
  upload=1
  shift
fi
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/dist/appstore}"
team="${TEAM_ID:-PA45BX35VE}"
profile="${PROFILE:-$root/packaging/macos/appstore.provisionprofile}"
build="${BUILD_NUMBER:-$(date -u +%Y%m%d%H%M)}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"

identity() {
  security find-identity -v "$@" | sed -n 's/.*"\(.*\)"$/\1/p' | grep "($team)"
}
app_id="$(identity -p codesigning | grep '^Apple Distribution' | head -1 || true)"
pkg_id="$(identity | grep -E '^(3rd Party Mac Developer Installer|Mac Installer Distribution)' | head -1 || true)"
[[ -n "$app_id" ]] || { echo "missing 'Apple Distribution' certificate for team $team" >&2; exit 1; }
[[ -n "$pkg_id" ]] || { echo "missing 'Mac Installer Distribution' certificate for team $team" >&2; exit 1; }
[[ -f "$profile" ]] || { echo "missing provisioning profile: $profile" >&2; exit 1; }

cd "$root"
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  MACOSX_DEPLOYMENT_TARGET=12.0 cargo build --release -p navdb-app --no-default-features --features appstore --target "$t"
done
mkdir -p "$out"
lipo -create -output "$out/navdb" target/aarch64-apple-darwin/release/navdb target/x86_64-apple-darwin/release/navdb

scripts/bundle-macos.sh "$out/navdb" "$out" "$version" >/dev/null
rm "$out/navdb"
app="$out/NavDB Studio.app"
plist="$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build" "$plist"
/usr/libexec/PlistBuddy -c "Set :LSMinimumSystemVersion 12.0" "$plist"
/usr/libexec/PlistBuddy -c "Add :ITSAppUsesNonExemptEncryption bool false" "$plist"
cp "$profile" "$app/Contents/embedded.provisionprofile"

ent="$(mktemp)"
sed "s/@TEAM_ID@/$team/g" packaging/macos/appstore.entitlements > "$ent"
codesign --force --sign "$app_id" --entitlements "$ent" --options runtime --timestamp=none "$app"
rm "$ent"
codesign --verify --strict --verbose=2 "$app"

pkg="$out/NavDB-Studio-appstore.pkg"
productbuild --component "$app" /Applications --sign "$pkg_id" "$pkg"
echo "$pkg (version $version, build $build)"

if [[ $upload == 1 ]]; then
  : "${ASC_KEY_ID:?set ASC_KEY_ID}" "${ASC_ISSUER_ID:?set ASC_ISSUER_ID}"
  key="${ASC_KEY_FILE:-$HOME/.config/appstoreconnect/AuthKey_$ASC_KEY_ID.p8}"
  xcrun altool --upload-package "$pkg" --api-key "$ASC_KEY_ID" --api-issuer "$ASC_ISSUER_ID" --p8-file-path "$key" --wait
fi
