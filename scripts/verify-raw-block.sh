#!/usr/bin/env bash
# Byte-verify that --raw-block produces the same archive contents as
# the default VFS path. Uses a modest single-exam workload
# (~1.7 GB / ~11 K files) so extractions fit under /tmp.
#
# Compares:
#   1. File list (names + uncompressed sizes) — deterministic.
#   2. Recursive content diff of the two extracted trees.
#
# Uses `-m store` and no password so both archives are byte-comparable
# in content after extraction (compression is deterministic too but
# AES adds per-file random salt, so we skip encryption for the diff).

set -euo pipefail

SOURCE="${VERIFY_SOURCE:-${1:-}}"
TMP="/tmp/tzip-verify"
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/tzip"

if [[ -z "$SOURCE" || ! -d "$SOURCE" ]]; then
    cat >&2 <<EOF
usage: $0 <source-directory>

  or set VERIFY_SOURCE in the environment.

  Runs tzip twice on the same source — once via the default VFS path,
  once via --raw-block — with -m store (no compression, no encryption)
  so the two archives are byte-comparable after extraction. Exits
  non-zero if file lists or content differ.
EOF
    exit 1
fi
if [[ ! -x "$BIN" ]]; then
    echo "tzip binary missing: $BIN (build with cargo build --release --features raw-apfs)" >&2
    exit 1
fi

rm -rf "$TMP"
mkdir -p "$TMP"

sudo -v || { echo "sudo auth failed" >&2; exit 1; }

echo "== workload: $SOURCE ($(du -sh "$SOURCE" | awk '{print $1}'))"
echo

echo "== default path =="
time sudo "$BIN" "$TMP/default.zip" "$SOURCE" \
    -m store \
    --exclude '._*' --exclude '.DS_Store' \
    -q
echo

echo "== --raw-block =="
time sudo "$BIN" "$TMP/raw.zip" "$SOURCE" \
    -m store \
    --exclude '._*' --exclude '.DS_Store' \
    --raw-block -q
echo

sudo chown "$USER" "$TMP/default.zip" "$TMP/raw.zip"

echo "== zip sizes =="
ls -lh "$TMP/default.zip" "$TMP/raw.zip"
echo

echo "== file list diff =="
unzip -l "$TMP/default.zip" | awk 'NR>3 && $1 ~ /^[0-9]+$/ {print $NF, $1}' | sort > "$TMP/default.list"
unzip -l "$TMP/raw.zip"     | awk 'NR>3 && $1 ~ /^[0-9]+$/ {print $NF, $1}' | sort > "$TMP/raw.list"
if diff -q "$TMP/default.list" "$TMP/raw.list" > /dev/null; then
    echo "FILE LISTS MATCH ($(wc -l < "$TMP/default.list") entries)"
else
    echo "!! FILE LISTS DIFFER — diff:"
    diff "$TMP/default.list" "$TMP/raw.list" | head -40
    exit 2
fi
echo

echo "== content diff =="
mkdir -p "$TMP/default-ext" "$TMP/raw-ext"
unzip -q "$TMP/default.zip" -d "$TMP/default-ext"
unzip -q "$TMP/raw.zip"     -d "$TMP/raw-ext"
if diff -rq "$TMP/default-ext" "$TMP/raw-ext" > "$TMP/content.diff"; then
    echo "CONTENT BYTE-IDENTICAL"
    rm "$TMP/content.diff"
else
    echo "!! CONTENT DIFFERS — first 40 lines of diff:"
    head -40 "$TMP/content.diff"
    echo
    echo "full report: $TMP/content.diff"
    exit 3
fi

echo
echo "== cleanup =="
rm -rf "$TMP/default-ext" "$TMP/raw-ext"
echo "kept archives + lists at $TMP for inspection"
