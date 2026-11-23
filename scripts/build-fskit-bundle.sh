#!/usr/bin/env bash
# build-fskit-bundle.sh
#
# Orchestrates the two-step build of the macOS FSKit extension bundle
# for `tzip mount`:
#
#   1. Rust: builds the tzip binary with --features fskit, and the
#      tzip-fskit-abi cdylib that exposes the extern "C" ABI.
#   2. Swift: builds the TzipFSExtension dylib (macos-fskit/ Swift
#      package) with `-Xlinker -undefined -Xlinker dynamic_lookup` so
#      the tzip_* symbols resolve at load time against the Rust dylib
#      we ship alongside it in the bundle.
#   3. Bundle assembly: produces `target/release/tzip.fs/` with the
#      standard Contents/Info.plist, MacOS/binary, Frameworks/dylib
#      layout; ad-hoc code sign so `sysextensionsctl install` accepts
#      it for local dev.
#
# This is a stand-alone script rather than a build.rs step because
# turning tzip's binary crate into a cdylib-exposing library would
# require promoting main.rs to a workspace layout — a refactor
# worth doing later but not in this PR.
#
# Prerequisites: macOS 15.4+ SDK, Xcode CLI tools (`swiftc`,
# `codesign`), and cargo. Run from the repo root.

set -euo pipefail

: "${PROFILE:=release}"

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

echo "==> Building tzip binary with --features fskit ($PROFILE)"
cargo build --features fskit --profile "$PROFILE"

echo "==> Building the Swift FSKit extension"
cd macos-fskit
# `release` is the only sensible configuration for an installed system
# extension; adjust if debugging locally.
swift build -c release
SWIFT_DYLIB="$(pwd)/.build/release/libTzipFSExtension.dylib"
if [[ ! -f "$SWIFT_DYLIB" ]]; then
  echo "error: expected Swift dylib at $SWIFT_DYLIB but did not find it." >&2
  exit 1
fi
cd "$REPO_ROOT"

echo "==> Assembling .fs bundle"
BUNDLE_DIR="target/$PROFILE/tzip.fs"
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Frameworks"

cp "$SWIFT_DYLIB" "$BUNDLE_DIR/Contents/MacOS/TzipFSExtension"

# Ship the Rust cdylib exporting the `tzip_*` ABI symbols alongside the
# Swift binary. The Swift dylib was linked with `-undefined dynamic_lookup`
# (see `macos-fskit/Package.swift`); at bundle-load time the loader
# finds these symbols in the dylib dropped next to it here.
RUST_DYLIB="target/$PROFILE/libtzip.dylib"
if [[ ! -f "$RUST_DYLIB" ]]; then
  echo "error: expected Rust cdylib at $RUST_DYLIB; did `cargo build --features fskit` run?" >&2
  exit 1
fi
cp "$RUST_DYLIB" "$BUNDLE_DIR/Contents/Frameworks/libtzip.dylib"

# Point the Swift binary at the dylib via its rpath so dyld finds the
# `tzip_*` symbols. `@loader_path/../Frameworks` is the standard macOS
# bundle layout convention.
install_name_tool -add_rpath "@loader_path/../Frameworks" \
    "$BUNDLE_DIR/Contents/MacOS/TzipFSExtension" 2>/dev/null || true

# And fix the dylib's own install name so it's referenced as a bundle-
# relative path (not an absolute target/release/... path from build time).
install_name_tool -id "@rpath/libtzip.dylib" \
    "$BUNDLE_DIR/Contents/Frameworks/libtzip.dylib"

cp macos-fskit/Sources/Resources/Info.plist "$BUNDLE_DIR/Contents/Info.plist"

echo "==> Ad-hoc code signing"
codesign --force --sign - "$BUNDLE_DIR/Contents/MacOS/TzipFSExtension"
codesign --force --sign - "$BUNDLE_DIR"

# The tzip binary itself must be signed with the
# `com.apple.developer.system-extension.install` entitlement so
# `OSSystemExtensionManager` accepts `tzip mount --install` requests
# at all. Ad-hoc signing with an entitlements plist is sufficient on a
# machine that has either (a) `systemextensionsctl developer on` set,
# or (b) a Developer ID; without either, macOS refuses the activation
# with "operation not permitted."
TZIP_BIN="target/$PROFILE/tzip"
if [[ -f "$TZIP_BIN" ]]; then
  echo "==> Signing tzip binary with SystemExtensions entitlement"
  codesign --force --sign - \
    --entitlements scripts/tzip.entitlements \
    --options runtime \
    "$TZIP_BIN"
fi

echo ""
echo "Bundle: $BUNDLE_DIR"
echo ""
echo "Status: Swift extension dylib + Rust cdylib are both in the bundle;"
echo "tzip_* symbol resolution should work at load time. Install the"
echo "bundle with \`tzip mount --install\` (PR #12) then mount via"
echo "\`tzip mount <archive> <mountpoint>\` (PR #13)."
