#!/usr/bin/env bash
# build-fskit-bundle.sh
#
# Builds the tzip FSKit extension in the modern ExtensionKit shape:
#
#   target/<profile>/tzip.app/
#     Contents/
#       Info.plist                        — host app
#       MacOS/tzip                        — Rust CLI binary (fskit feature)
#       Frameworks/libtzip.dylib          — Rust cdylib (tzip_* ABI symbols)
#       embedded.provisionprofile         — host app Developer ID profile
#       PlugIns/tzip.appex/
#         Contents/
#           Info.plist                    — XPC!, EXAppExtensionAttributes
#           MacOS/TzipFSExtension         — Swift executable (@main)
#           Frameworks/libtzip.dylib      — same Rust cdylib, dup for @rpath
#           embedded.provisionprofile     — .appex Developer ID profile
#
# Signs the whole tree inside-out with your Developer ID Application
# cert when `DEVELOPER_ID_APPLICATION` is in the env (hardened runtime
# + timestamp). Without it, ad-hoc signed — useful only on SIP-off
# dev boxes.
#
# Bundle ids default to the project's `io.qbio.tzip*` namespace.
# Forks that signed under a different Apple Developer team override
# via `TZIP_HOST_BUNDLE_ID` + `TZIP_FSKIT_BUNDLE_ID` so you don't
# have to patch tracked files in-tree. The entitlements templates
# in `scripts/tzip.*.entitlements.in` get substituted the same way
# (`TEAM_ID`, `HOST_BUNDLE_ID`, `FSKIT_BUNDLE_ID` placeholders).
#
# Follow with `scripts/notarize-fskit-bundle.sh` to send the result
# to Apple notarisation.
#
# Full worked walkthrough — including the Apple Developer Portal
# dance for provisioning profiles — lives in `macos-fskit/README.md`.

set -euo pipefail

: "${PROFILE:=release}"
: "${TZIP_HOST_BUNDLE_ID:=io.qbio.tzip}"
: "${TZIP_FSKIT_BUNDLE_ID:=io.qbio.tzip.fskit}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this script only runs on macOS." >&2
  exit 1
fi
if ! command -v swiftc >/dev/null; then
  echo "error: swiftc not in PATH. Install Xcode CLI tools." >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Extract team id from the Developer ID cert when present, so the
# entitlements templates substitute the right `AHQ335DYV4.<bundleid>`
# application-identifier. Fallback empty string = ad-hoc path.
TEAM_ID=""
if [[ -n "${DEVELOPER_ID_APPLICATION:-}" ]]; then
  TEAM_ID="$(echo "$DEVELOPER_ID_APPLICATION" | sed -nE 's/.*\(([A-Z0-9]{10})\).*/\1/p')"
fi

echo "==> Building tzip (--features fskit, $PROFILE)"
cargo build --features fskit --profile "$PROFILE"

echo "==> Building Swift .appex executable"
# Swift needs the Rust cdylib's directory on `-L` to link `-ltzip`;
# pass it in via env var so Package.swift can pick it up.
export TZIP_RUST_LIBDIR="$REPO_ROOT/target/$PROFILE"
(cd macos-fskit && swift build -c release --disable-sandbox)
SWIFT_EXE="$REPO_ROOT/macos-fskit/.build/release/TzipFSExtension"
if [[ ! -f "$SWIFT_EXE" ]]; then
  echo "error: Swift executable not found at $SWIFT_EXE." >&2
  exit 1
fi

APP="target/$PROFILE/tzip.app"
# `Contents/PlugIns/` is where every user-installed `.appex` lives
# (Launch Services' convention). System-wide FSKit extensions
# under `/System/Library/ExtensionKit/Extensions/` are the exception,
# not the rule.
APPEX="$APP/Contents/PlugIns/tzip.appex"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" \
         "$APP/Contents/Frameworks" \
         "$APPEX/Contents/MacOS" \
         "$APPEX/Contents/Frameworks"

# ── Host app Info.plist (substituted with the configured host id) ──
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>    <string>en</string>
  <key>CFBundleExecutable</key>           <string>tzip</string>
  <key>CFBundleIdentifier</key>           <string>$TZIP_HOST_BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key>                 <string>tzip</string>
  <key>CFBundlePackageType</key>          <string>APPL</string>
  <key>CFBundleShortVersionString</key>   <string>0.2.0</string>
  <key>CFBundleVersion</key>              <string>1</string>
  <key>LSMinimumSystemVersion</key>       <string>15.4</string>
  <key>LSUIElement</key>                  <true/>
</dict>
</plist>
PLIST

cp "target/$PROFILE/tzip" "$APP/Contents/MacOS/tzip"
RUST_DYLIB="target/$PROFILE/libtzip.dylib"
if [[ ! -f "$RUST_DYLIB" ]]; then
  echo "error: $RUST_DYLIB missing — did cargo build --features fskit run?" >&2
  exit 1
fi
cp "$RUST_DYLIB" "$APP/Contents/Frameworks/libtzip.dylib"

# ── Extension bundle (.appex) ──
# Substitute the configured fskit bundle id into the committed
# Info.plist template so forks don't have to edit tracked files.
sed "s|<string>io\\.qbio\\.tzip\\.fskit</string>|<string>$TZIP_FSKIT_BUNDLE_ID</string>|" \
    macos-fskit/Sources/Resources/Info.plist > "$APPEX/Contents/Info.plist"
cp "$SWIFT_EXE" "$APPEX/Contents/MacOS/TzipFSExtension"
cp "$RUST_DYLIB" "$APPEX/Contents/Frameworks/libtzip.dylib"

install_name_tool -add_rpath "@loader_path/../Frameworks" \
  "$APPEX/Contents/MacOS/TzipFSExtension" 2>/dev/null || true
install_name_tool -id "@rpath/libtzip.dylib" \
  "$APPEX/Contents/Frameworks/libtzip.dylib"
install_name_tool -id "@rpath/libtzip.dylib" \
  "$APP/Contents/Frameworks/libtzip.dylib"

# Swift build linked the executable against libtzip.dylib by its
# build-tree absolute path (`target/release/build/.../libtzip.dylib`).
# Rewrite that LC_LOAD_DYLIB to `@rpath/libtzip.dylib` so the rpath
# we just added resolves at runtime.
BUILDTREE_DYLIB="$(otool -L "$APPEX/Contents/MacOS/TzipFSExtension" \
  | awk '/libtzip\.dylib/ && !/@rpath/ {print $1; exit}')"
if [[ -n "$BUILDTREE_DYLIB" ]]; then
  install_name_tool -change "$BUILDTREE_DYLIB" "@rpath/libtzip.dylib" \
    "$APPEX/Contents/MacOS/TzipFSExtension"
fi

# ── Embed provisioning profiles ──
# Both the host and the .appex need their own profile (restricted
# entitlements). Call sites pass:
#   TZIP_PROVISION_PROFILE_HOST  — host (`com.apple.developer.system-extension.install`)
#   TZIP_PROVISION_PROFILE_APPEX — extension (`com.apple.developer.fskit.fsmodule`)
if [[ -n "${TZIP_PROVISION_PROFILE_HOST:-}" && -f "$TZIP_PROVISION_PROFILE_HOST" ]]; then
  cp "$TZIP_PROVISION_PROFILE_HOST" "$APP/Contents/embedded.provisionprofile"
  echo "==> Embedded host profile: $TZIP_PROVISION_PROFILE_HOST"
fi
if [[ -n "${TZIP_PROVISION_PROFILE_APPEX:-}" && -f "$TZIP_PROVISION_PROFILE_APPEX" ]]; then
  cp "$TZIP_PROVISION_PROFILE_APPEX" "$APPEX/Contents/embedded.provisionprofile"
  echo "==> Embedded .appex profile: $TZIP_PROVISION_PROFILE_APPEX"
fi

# ── Substitute entitlements templates ──
# The committed `.entitlements.in` templates have `__TEAM_ID__`,
# `__HOST_BUNDLE_ID__`, `__FSKIT_BUNDLE_ID__` placeholders. Render
# them into the build-tree so codesign has concrete files to seal.
render_entitlements() {
  local src="$1" dst="$2"
  sed -e "s|__TEAM_ID__|${TEAM_ID}|g" \
      -e "s|__HOST_BUNDLE_ID__|${TZIP_HOST_BUNDLE_ID}|g" \
      -e "s|__FSKIT_BUNDLE_ID__|${TZIP_FSKIT_BUNDLE_ID}|g" \
      "$src" > "$dst"
}
mkdir -p "target/$PROFILE/entitlements"
HOST_ENT="target/$PROFILE/entitlements/tzip-app.entitlements"
APPEX_ENT="target/$PROFILE/entitlements/tzip-appex.entitlements"
render_entitlements scripts/tzip-app.entitlements.in  "$HOST_ENT"
render_entitlements scripts/tzip-appex.entitlements.in "$APPEX_ENT"

# ── Signing ──
if [[ -n "${DEVELOPER_ID_APPLICATION:-}" ]]; then
  IDENTITY="$DEVELOPER_ID_APPLICATION"
  OPTS=(--options runtime --timestamp)
  echo "==> Developer-ID signing ($IDENTITY)"
else
  IDENTITY="-"
  OPTS=()
  echo "==> Ad-hoc code signing (dev only — won't pass stock SIP)"
fi

# Sign inside-out so outer signatures seal the inner state.
codesign --force --sign "$IDENTITY" "${OPTS[@]}" \
  "$APPEX/Contents/Frameworks/libtzip.dylib"
codesign --force --sign "$IDENTITY" "${OPTS[@]}" \
  --entitlements "$APPEX_ENT" \
  "$APPEX/Contents/MacOS/TzipFSExtension"
codesign --force --sign "$IDENTITY" "${OPTS[@]}" \
  --entitlements "$APPEX_ENT" \
  "$APPEX"

codesign --force --sign "$IDENTITY" "${OPTS[@]}" \
  "$APP/Contents/Frameworks/libtzip.dylib"
codesign --force --sign "$IDENTITY" "${OPTS[@]}" \
  --entitlements "$HOST_ENT" \
  "$APP/Contents/MacOS/tzip"
codesign --force --sign "$IDENTITY" "${OPTS[@]}" \
  --entitlements "$HOST_ENT" \
  "$APP"

echo "==> Verify"
codesign --verify --strict --verbose=2 --deep "$APP"

echo ""
echo "App:       $APP"
echo "Extension: $APPEX"
echo ""
echo "Next:"
echo "  ./scripts/notarize-fskit-bundle.sh    # → stapled .app"
echo "  cp -R $APP /Applications/tzip.app"
echo "  open 'x-apple.systempreferences:com.apple.LoginItems-Settings.extension'"
echo "    → scroll to 'tzip Extensions' → flip 'FSKit Modules' on"
echo "  /Applications/tzip.app/Contents/MacOS/tzip mount s3://bucket/archive.zip /tmp/mnt -p PASSWORD"
