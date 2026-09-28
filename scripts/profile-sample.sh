#!/usr/bin/env bash
# Capture a 60-second `sample` profile of tzip during the read hot
# path. Output is a human-readable call tree.
#
# Usage:
#   PROFILE_SOURCE=<dir> [PROFILE_PASSWORD=<file>] $0
#   $0 <source-dir>

set -euo pipefail

OUTDIR="/tmp/tzip-profiles"
mkdir -p "$OUTDIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$OUTDIR/sample-$STAMP.txt"
ZIP="$OUTDIR/scratch-$STAMP.zip"

SOURCE="${PROFILE_SOURCE:-${1:-}}"
PWDFILE="${PROFILE_PASSWORD:-}"

if [[ -z "$SOURCE" || ! -d "$SOURCE" ]]; then
    echo "usage: $0 <source-directory>" >&2
    echo "  (or set PROFILE_SOURCE in env)" >&2
    exit 1
fi

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/profiling/tzip"
if [[ ! -x "$BIN" ]]; then
    BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/tzip"
    echo "warning: profiling build missing, using release (symbols stripped)" >&2
    echo "         build with: cargo build --profile profiling --features raw-apfs" >&2
fi

sudo -v || { echo "sudo auth failed" >&2; exit 1; }

echo "== workload: $SOURCE"
echo "== sample:   $OUT"
echo

PWD_ARG=()
[[ -n "$PWDFILE" ]] && PWD_ARG=(--password-file "$PWDFILE")
sudo "$BIN" "$ZIP" "$SOURCE" \
    -m deflate -x 9 \
    "${PWD_ARG[@]}" \
    --exclude '._*' --exclude '.DS_Store' \
    --raw-block -q &
SUDO_PID=$!

# Wait for the read stage to start (after prewalk). Prewalk prints its
# line at ~1-2s; give it a beat, then attach `sample` for 60s. If tzip
# finishes before then, sample stops early — that's fine.
sleep 3
# Match on process comm (basename), not on command line — a `sudo tzip`
# invocation shows tzip's path in its argv so plain `pgrep -f` will
# also match sudo itself.
PID=""
for candidate in $(pgrep -f "$BIN"); do
    comm="$(ps -p "$candidate" -o comm= 2>/dev/null || true)"
    if [[ "$(basename "$comm")" == "tzip" ]]; then
        PID="$candidate"
        break
    fi
done
if [[ -z "$PID" ]]; then
    echo "tzip PID not found (may have finished already)"
    wait "$SUDO_PID" || true
    exit 1
fi

echo "sampling pid $PID for 60s..."
sudo sample "$PID" 60 -mayDie -f "$OUT" 2>&1 | tail -5 || true

wait "$SUDO_PID" || true
sudo chown "$USER" "$OUT" 2>/dev/null || true
echo
echo "sample: $OUT"
