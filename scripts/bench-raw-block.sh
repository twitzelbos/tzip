#!/usr/bin/env bash
# Side-by-side timing of tzip default path vs --raw-block on a chosen
# source. Useful for comparing drives / datasets — in particular,
# fresh-populated drives (where files-in-a-dir share adjacent disk
# offsets, so extent-order coalescing actually helps) vs
# aged-populated drives (files scattered across the volume).
#
# Usage:
#   sudo ./scripts/bench-raw-block.sh <source-path> [runs]
#
# Both runs use -m store (isolates read throughput from compression
# CPU cost) and no password (no encryption overhead).
#
# Prints:
#   * time for default VFS path
#   * time for --raw-block path
#   * speedup ratio
#   * bulk stats snippet (extents/run, coalesced bytes) — hints at
#     how clustered the source's on-disk layout is

set -euo pipefail

SOURCE="${1:-}"
RUNS="${2:-1}"
if [[ -z "$SOURCE" || ! -e "$SOURCE" ]]; then
    echo "usage: sudo $0 <source-path> [runs=1]" >&2
    echo "  source-path: a directory or file to archive" >&2
    exit 1
fi

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/tzip"
if [[ ! -x "$BIN" ]]; then
    echo "tzip binary missing: $BIN" >&2
    echo "  build with: cargo build --release --features raw-apfs" >&2
    exit 1
fi

TMP="/tmp/tzip-bench"
mkdir -p "$TMP"
SIZE_HUMAN="$(du -sh "$SOURCE" 2>/dev/null | awk '{print $1}')"
FILE_COUNT="$(find "$SOURCE" -type f 2>/dev/null | wc -l | tr -d ' ')"

sudo -v || { echo "sudo auth failed" >&2; exit 1; }

echo "== bench: $SOURCE ($SIZE_HUMAN, $FILE_COUNT files)"
echo

now_epoch() {
    # macOS `date` has no %N; use python for subsecond wall time.
    python3 -c 'import time; print(f"{time.time():.3f}")'
}

run_and_time() {
    local label="$1"; shift
    local out="$TMP/bench-$$-$label.zip"
    rm -f "$out"
    local start end elapsed
    start=$(now_epoch)
    # Filter tzip's output to just the interesting lines and send it to
    # STDERR so it prints without contaminating this function's return
    # value (command substitution captures stdout only).
    sudo "$BIN" "$out" "$SOURCE" \
        -m store --exclude '._*' --exclude '.DS_Store' \
        "$@" -v 2>&1 | grep -E 'raw-block|auto-tuned' 1>&2 || true
    end=$(now_epoch)
    elapsed=$(echo "$end - $start" | bc)
    rm -f "$out"
    echo "$elapsed"
}

default_times=()
raw_times=()
for i in $(seq 1 "$RUNS"); do
    echo "--- run $i/$RUNS: default (VFS) ---"
    dt=$(run_and_time "default-$i" --raw-block=false)
    printf "default: %.2fs\n\n" "$dt"
    default_times+=("$dt")

    echo "--- run $i/$RUNS: --raw-block ---"
    rt=$(run_and_time "raw-$i" --raw-block)
    printf "raw-block: %.2fs\n\n" "$rt"
    raw_times+=("$rt")
done

# Averages
avg() {
    local sum=0 n=$#
    for x in "$@"; do sum=$(echo "$sum + $x" | bc); done
    echo "scale=2; $sum / $n" | bc
}

d_avg=$(avg "${default_times[@]}")
r_avg=$(avg "${raw_times[@]}")
speedup=$(echo "scale=2; $d_avg / $r_avg" | bc)

echo "=== summary ==="
echo "  source:         $SOURCE ($SIZE_HUMAN, $FILE_COUNT files, $RUNS runs each)"
echo "  default (VFS):  avg ${d_avg}s"
echo "  --raw-block:    avg ${r_avg}s"
echo "  speedup:        ${speedup}×"
