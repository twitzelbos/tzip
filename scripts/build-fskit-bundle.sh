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

# TODO (future PR): emit the Rust-side cdylib (`libtzip_fskit_abi.dylib`)
# alongside and add it to the bundle's Frameworks directory. Right now
# the tzip binary doesn't expose a cdylib target — refactoring to a
# workspace with a dedicated `tzip-fskit-abi` crate is the follow-up.
# Without this dylib, the Swift extension can't actually load (undefined
# `tzip_*` symbols at runtime). The scaffolding compiles; the mount
# runtime does not yet work end-to-end.

cp macos-fskit/Sources/Resources/Info.plist "$BUNDLE_DIR/Contents/Info.plist"

echo "==> Ad-hoc code signing"
codesign --force --sign - "$BUNDLE_DIR/Contents/MacOS/TzipFSExtension"
codesign --force --sign - "$BUNDLE_DIR"

echo ""
echo "Bundle: $BUNDLE_DIR"
echo ""
echo "Status: SCAFFOLDING. The Swift extension dylib links with"
echo "dynamic_lookup for its tzip_* symbols; the Rust-side cdylib that"
echo "resolves them is NOT bundled yet (pending workspace refactor)."
echo "Mount runtime will fail with \"symbol not found\" until that PR"
echo "lands. The Swift module + Keychain wrapper + ABI are proven to"
echo "compile against the real FSKit SDK."
