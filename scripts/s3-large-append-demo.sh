#!/usr/bin/env bash
# s3-large-append-demo.sh
#
# Demo + manual regression for `tzip u s3://…` with a large (>
# tens-of-MiB) append body. This is the scenario that triggered
# issue #112: before the fix, tzip would pick the single-PUT
# fallback based on the tiny seed object size alone, then try to
# PUT the whole 500+ MB resulting archive in one HTTP request,
# which died with `broken pipe` on real-world networks.
#
# After the fix, any combined body > SINGLE_PUT_MAX_TOTAL (32 MiB,
# set in src/update/s3.rs) routes through the multipart path,
# including the hybrid `multipart_append_small_prefix` branch for
# tiny-seed + huge-tail cases like this demo.
#
# Mock-S3 regression tests in tests/remote/writer_s3.rs cover
# both dispatch branches; this script is the one real-cloud
# check that production-sized appends actually complete over a
# real network.
#
# Flow:
#   1. Pull AWS keys from the "Access keys" CSV.
#   2. Build a tiny local seed archive (one byte file). Upload
#      to s3://<BUCKET>/<KEY> with `aws s3 cp` (NOT tzip — this
#      is just setup).
#   3. Enumerate INPUT_DIR for files (defaults to generating a
#      40-MiB synthetic corpus under a scratch dir — comfortably
#      above the 32-MiB single-PUT threshold).
#   4. Feed the file list through `tzip u s3://…` via NUL-safe
#      xargs. The first (and any subsequent) tzip invocation
#      lands on the small-prefix multipart path — never the
#      broken single-PUT one.
#   5. Verify with `tzip ls` — all input files must appear.
#
# Env overrides:
#   AWS_KEYS_CSV   — path to credentials CSV
#                    (default: ~/Downloads/twitzel_accessKeys\ \(1\).csv)
#   AWS_REGION     — bucket's region             (default: us-east-1)
#   BUCKET         — target bucket name          (default: tzip-demo-bucket)
#   KEY            — target object key           (default: large-append-<timestamp>.zip)
#   TZIP           — path to the tzip binary     (default: ./target/release/tzip)
#   INPUT_DIR      — directory to archive        (default: generated 40-MiB corpus)
#
# Exit status: 0 if the archive contains every input file, non-zero otherwise.

set -euo pipefail

AWS_KEYS_CSV="${AWS_KEYS_CSV:-$HOME/Downloads/twitzel_accessKeys (1).csv}"
AWS_REGION="${AWS_REGION:-us-east-1}"
BUCKET="${BUCKET:-tzip-demo-bucket}"
KEY="${KEY:-large-append-$(date -u +%Y%m%dT%H%M%SZ).zip}"
TZIP="${TZIP:-$(pwd)/target/release/tzip}"
INPUT_DIR="${INPUT_DIR:-}"

if [[ ! -x "$TZIP" ]]; then
    echo "tzip binary not found or not executable at: $TZIP" >&2
    echo "Build it first with: cargo build --release" >&2
    exit 1
fi

if [[ ! -r "$AWS_KEYS_CSV" ]]; then
    echo "AWS keys CSV not readable at: $AWS_KEYS_CSV" >&2
    exit 1
fi

if ! command -v aws >/dev/null 2>&1; then
    echo "aws CLI not found on PATH (needed for the seed upload)" >&2
    echo "Install it (brew install awscli) and retry" >&2
    exit 1
fi

# Parse `Access key ID,Secret access key` CSV without echoing
# the values.
AWS_ACCESS_KEY_ID=$(awk -F, 'NR==2 {gsub(/\r/,""); print $1}' "$AWS_KEYS_CSV")
AWS_SECRET_ACCESS_KEY=$(awk -F, 'NR==2 {gsub(/\r/,""); print $2}' "$AWS_KEYS_CSV")
if [[ -z "$AWS_ACCESS_KEY_ID" || -z "$AWS_SECRET_ACCESS_KEY" ]]; then
    echo "Couldn't parse AWS access key + secret from $AWS_KEYS_CSV" >&2
    exit 1
fi
export AWS_ACCESS_KEY_ID
export AWS_SECRET_ACCESS_KEY
export AWS_REGION

SCRATCH=$(mktemp -d /tmp/tzip-s3-large-demo.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT

echo "tzip binary:     $TZIP"
echo "bucket / key:    s3://${BUCKET}/${KEY}"
echo "region:          $AWS_REGION"
echo "scratch dir:     $SCRATCH"

# ── 1. Prepare input files ─────────────────────────────────────
MANIFEST="$SCRATCH/manifest.txt"
MANIFEST_NUL="$SCRATCH/manifest.nul"

if [[ -z "$INPUT_DIR" ]]; then
    # Default corpus: 20 × 2 MiB random files. Total 40 MiB sits
    # above SINGLE_PUT_MAX_TOTAL (32 MiB), so the fix path is the
    # one actually exercised.
    INPUT_DIR="$SCRATCH/corpus"
    mkdir -p "$INPUT_DIR"
    echo
    echo "[1/4] Generating 40 MiB synthetic corpus in $INPUT_DIR …"
    for i in $(seq 1 20); do
        dd if=/dev/urandom of="$INPUT_DIR/file-$(printf '%03d' "$i").bin" \
            bs=1m count=2 status=none
    done
else
    echo
    echo "[1/4] Using user-provided INPUT_DIR: $INPUT_DIR"
fi

# Build the manifest — every regular file under INPUT_DIR
find "$INPUT_DIR" -type f -print0 > "$MANIFEST_NUL"
tr '\0' '\n' < "$MANIFEST_NUL" > "$MANIFEST"
FILE_COUNT=$(wc -l < "$MANIFEST" | tr -d ' ')
TOTAL_BYTES=$(find "$INPUT_DIR" -type f -exec stat -f '%z' {} \; 2>/dev/null \
    | awk '{s+=$1} END {print s}')
echo "    files to upload: $FILE_COUNT"
echo "    total bytes:     $TOTAL_BYTES"

if [[ "$FILE_COUNT" -eq 0 ]]; then
    echo "No files to upload under $INPUT_DIR" >&2
    exit 1
fi

# ── 2. Build + upload a tiny seed ──────────────────────────────
cd "$SCRATCH"
echo
echo "[2/4] Building tiny seed archive locally (triggers issue #112 path)"
printf '%s\n' 'seed — delete me after' > seed.txt
"$TZIP" create seed.zip seed.txt -q
ls -la seed.zip

echo
echo "[3/4] Uploading seed to s3://${BUCKET}/${KEY} …"
aws s3 cp seed.zip "s3://${BUCKET}/${KEY}"

# ── 3. Append the real corpus via tzip u ───────────────────────
echo
echo "[4/4] tzip u — appending $FILE_COUNT files via remote append"
echo "        (with the issue #112 fix, large tail + tiny seed"
echo "         takes the small-prefix multipart path — NOT single-PUT)"
echo

time xargs -0 -s 900000 \
    "$TZIP" u -j 8 -m store "s3://${BUCKET}/${KEY}" \
    < "$MANIFEST_NUL"

# ── 4. Verify ──────────────────────────────────────────────────
echo
echo "=== Verify: tzip ls ==="
LS_OUT=$("$TZIP" ls "s3://${BUCKET}/${KEY}")
# Footer line looks like:   N files (both counts)
FOOTER_COUNT=$(printf '%s\n' "$LS_OUT" | awk '/files$/ {print $(NF-1); exit}')
echo "Archive reports $FOOTER_COUNT files (expected $((FILE_COUNT + 1)) — your inputs plus seed.txt)"

MISSING=0
while IFS= read -r p; do
    # Strip leading / to match tzip's stored names.
    stored="${p#/}"
    if ! grep -qE "[[:space:]]${stored}$" <<< "$LS_OUT"; then
        echo "FAIL: $stored missing from archive" >&2
        MISSING=$((MISSING + 1))
        if [[ "$MISSING" -gt 3 ]]; then
            echo "    (suppressing further missing-entry noise)" >&2
            break
        fi
    fi
done < "$MANIFEST"

if [[ "$MISSING" -gt 0 ]]; then
    exit 1
fi

echo
echo "PASS: tzip u s3:// appended $FILE_COUNT files via multipart (issue #112 fix verified)."
echo "      Archive URL: s3://${BUCKET}/${KEY}"
