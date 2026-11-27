#!/usr/bin/env bash
# notarize-fskit-bundle.sh — submit the signed tzip.fs + tzip binary
# to Apple notarisation, wait for the result, and staple tickets
# locally so stock SIP-enabled Macs can install + mount without
# any user system surgery.
#
# Prerequisites (one-time setup on this Mac):
#   xcrun notarytool store-credentials "notarytool-profile" \
#     --apple-id "<apple id email>" \
#     --team-id  "<10-char team id>"
#   # Enter the app-specific password from appleid.apple.com → App-Specific Passwords.
#
# Then, after `build-fskit-bundle.sh` has produced a Developer-ID-
# signed bundle:
#   DEVELOPER_ID_APPLICATION="Developer ID Application: Your Name (TEAMID)" \
#     ./scripts/build-fskit-bundle.sh
#   ./scripts/notarize-fskit-bundle.sh
#
# Usage:
#   notarize-fskit-bundle.sh [--profile <keychain-profile>]
#
# `--profile` defaults to "notarytool-profile".
set -euo pipefail

PROFILE_NAME="notarytool-profile"
BUILD_PROFILE="${PROFILE:-release}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) PROFILE_NAME="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BUNDLE="target/$BUILD_PROFILE/tzip.fs"
TZIP_BIN="target/$BUILD_PROFILE/tzip"
if [[ ! -d "$BUNDLE" ]]; then
  echo "error: $BUNDLE missing — run build-fskit-bundle.sh with DEVELOPER_ID_APPLICATION set first." >&2
  exit 1
fi

# Verify Developer-ID signing before wasting a notarisation round.
# `codesign --verify` exits 0 on a valid signature.
echo "==> Verifying Developer-ID signature on $BUNDLE"
if ! codesign --verify --strict --verbose=2 "$BUNDLE"; then
  echo "error: codesign verification failed — rebuild with DEVELOPER_ID_APPLICATION set." >&2
  exit 1
fi

# Notarisation requires a zipped payload. `ditto` preserves symlinks
# and extended attributes the way Apple's tooling expects.
ZIP_PATH="$REPO_ROOT/target/$BUILD_PROFILE/tzip.fs.zip"
rm -f "$ZIP_PATH"
echo "==> Zipping bundle for submission → $ZIP_PATH"
ditto -c -k --sequesterRsrc --keepParent "$BUNDLE" "$ZIP_PATH"

echo "==> Submitting to Apple notarytool (profile: $PROFILE_NAME)"
echo "    This typically takes 2-10 minutes."
if ! xcrun notarytool submit "$ZIP_PATH" \
       --keychain-profile "$PROFILE_NAME" \
       --wait; then
  echo "error: notarisation failed; check the log via" >&2
  echo "  xcrun notarytool log <submission-id> --keychain-profile $PROFILE_NAME" >&2
  exit 1
fi

# Staple the notarisation ticket onto the bundle so offline installs
# still see "notarised" without re-checking Apple's servers.
echo "==> Stapling notarisation ticket onto $BUNDLE"
xcrun stapler staple "$BUNDLE"
xcrun stapler validate "$BUNDLE"

# The tzip binary is a Mach-O (not a bundle), so it can't carry a
# stapled ticket of its own, but notarytool accepts it as a
# zipped payload for the Developer-ID-signature registration.
if [[ -f "$TZIP_BIN" ]]; then
  echo "==> Submitting tzip binary separately (binaries can't be stapled)"
  BIN_ZIP="$REPO_ROOT/target/$BUILD_PROFILE/tzip.binary.zip"
  rm -f "$BIN_ZIP"
  ditto -c -k --keepParent "$TZIP_BIN" "$BIN_ZIP"
  xcrun notarytool submit "$BIN_ZIP" \
    --keychain-profile "$PROFILE_NAME" \
    --wait
fi

echo ""
echo "✓ Bundle at $BUNDLE is signed, notarised, stapled."
echo "  Install with: ./$TZIP_BIN mount --install"
echo "  Mount with:   ./$TZIP_BIN mount s3://bucket/key.zip /mnt -p <password>"
