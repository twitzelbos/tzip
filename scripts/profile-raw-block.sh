#!/usr/bin/env bash
# Profile a --raw-block tzip run against a short workload (~50s).
# Produces an Instruments .trace file you can open with `open`.
#
# Uses attach-to-pid so we can keep `sudo` on the tzip side while
# letting xctrace do sampling from a normal shell.
#
# Usage: ./scripts/profile-raw-block.sh [template]
#   template: "Time Profiler" (default), "System Trace", "CPU Counters"

set -euo pipefail

TEMPLATE="${1:-Time Profiler}"
OUTDIR="/tmp/tzip-profiles"
mkdir -p "$OUTDIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
TRACE="$OUTDIR/tzip-$STAMP.trace"
ZIP="$OUTDIR/scratch-$STAMP.zip"

SOURCE="${PROFILE_SOURCE:-${2:-}}"
PWDFILE="${PROFILE_PASSWORD:-}"

if [[ -z "$SOURCE" || ! -d "$SOURCE" ]]; then
    cat >&2 <<EOF
usage: $0 [template] <source-directory>

  or set PROFILE_SOURCE and (optionally) PROFILE_PASSWORD in env.
  template defaults to "Time Profiler".
EOF
    exit 1
fi

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/tzip"
if [[ ! -x "$BIN" ]]; then
    echo "binary missing (build with: cargo build --release --features raw-apfs): $BIN" >&2
    exit 1
fi

echo "== profile:  $TEMPLATE"
echo "== workload: $SOURCE"
echo "== trace:    $TRACE"
echo "== output:   $ZIP"
echo

# Start tzip in the background under sudo, then attach the profiler
# once we know the PID. `--time-limit` caps profile duration; tzip
# finishes on its own and xctrace stops when it does or at the limit.
# Prompt for sudo credentials up front so the password prompt doesn't
# eat our polling window later.
sudo -v || { echo "sudo auth failed" >&2; exit 1; }

echo "starting tzip (sudo)..."
PWD_ARG=()
[[ -n "$PWDFILE" ]] && PWD_ARG=(--password-file "$PWDFILE")
sudo "$BIN" "$ZIP" "$SOURCE" \
    -m deflate -x 9 \
    "${PWD_ARG[@]}" \
    --exclude '._*' --exclude '.DS_Store' \
    --raw-block -q &
SUDO_PID=$!

# Wait for the real tzip PID to appear. It's a descendant (usually a
# direct child) of the sudo process. Use a generous window; the sudo
# fork+exec can take a moment.
PID=""
for _ in $(seq 1 120); do
    sleep 0.25
    PID="$(pgrep -f "$BIN" | grep -v "^$SUDO_PID$" | head -1 || true)"
    [[ -n "$PID" ]] && break
done
if [[ -z "$PID" ]]; then
    echo "couldn't find tzip PID under sudo (pid $SUDO_PID)"
    echo "current processes matching tzip:"
    ps -A -o pid,ppid,command | grep -E 'target/release/tzip' | grep -v grep || true
    kill "$SUDO_PID" 2>/dev/null || true
    exit 1
fi
echo "attaching xctrace to pid $PID..."

# Record until tzip exits (attach mode stops when target exits).
# System Trace needs sudo; Time Profiler doesn't, but sudo is fine either way.
sudo xcrun xctrace record --template "$TEMPLATE" \
    --attach "$PID" --time-limit 120s \
    --output "$TRACE" 2>&1 | tail -20 || true

wait "$SUDO_PID" || true
echo
echo "trace: $TRACE"
echo "open with: open '$TRACE'"
