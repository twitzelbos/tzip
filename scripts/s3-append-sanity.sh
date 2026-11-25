#!/usr/bin/env bash
# s3-append-sanity.sh
#
# Manual verification that `tzip u s3://…` (issue #68 / PR #70)
# actually works end-to-end against real AWS S3. CI exercises
# the code paths against a mock; this script is the one check
# that real-cloud behaviour matches — the one thing a mock can't
# tell you.
#
# Flow:
#   1. Pull `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` from
#      the "Access keys" CSV AWS hands you when you create an
#      IAM access key (header line + one data row).
#   2. Seed `s3://<BUCKET>/<KEY>` with a tiny two-entry archive
#      via `aws s3 cp`.
#   3. Append a third entry with `tzip u` straight against
#      the s3:// URL — the behaviour PR #70 added.
#   4. Verify with `tzip ls` + `tzip cat`.
#
# Env overrides:
#   AWS_KEYS_CSV   — path to the credentials CSV
#                    (default: ~/Downloads/twitzel_accessKeys\ \(1\).csv)
#   AWS_REGION     — bucket's region             (default: us-east-1)
#   BUCKET         — target bucket name          (default: tzip-demo-bucket)
#   KEY            — target object key           (default: append-test.zip)
#   TZIP           — path to the tzip binary     (default: ./target/release/tzip)
#
# Exit status: 0 if everything verifies, non-zero otherwise.

set -euo pipefail

AWS_KEYS_CSV="${AWS_KEYS_CSV:-$HOME/Downloads/twitzel_accessKeys (1).csv}"
AWS_REGION="${AWS_REGION:-us-east-1}"
BUCKET="${BUCKET:-tzip-demo-bucket}"
KEY="${KEY:-append-test.zip}"
TZIP="${TZIP:-$(pwd)/target/release/tzip}"

if [[ ! -x "$TZIP" ]]; then
    echo "tzip binary not found or not executable at: $TZIP" >&2
    echo "Build it first with: cargo build --release" >&2
    exit 1
fi

if [[ ! -r "$AWS_KEYS_CSV" ]]; then
    echo "AWS keys CSV not readable at: $AWS_KEYS_CSV" >&2
    exit 1
fi

# Parse `Access key ID,Secret access key` CSV without echoing
# the values. `awk -F,` on the second row keeps both fields in
# process memory; we assign them to env vars directly.
AWS_ACCESS_KEY_ID=$(awk -F, 'NR==2 {gsub(/\r/,""); print $1}' "$AWS_KEYS_CSV")
AWS_SECRET_ACCESS_KEY=$(awk -F, 'NR==2 {gsub(/\r/,""); print $2}' "$AWS_KEYS_CSV")
if [[ -z "$AWS_ACCESS_KEY_ID" || -z "$AWS_SECRET_ACCESS_KEY" ]]; then
    echo "Couldn't parse AWS access key + secret from $AWS_KEYS_CSV" >&2
    echo "Expected a CSV with a header row and one data row of" >&2
    echo "  <access_key>,<secret_key>" >&2
    exit 1
fi
export AWS_ACCESS_KEY_ID
export AWS_SECRET_ACCESS_KEY
export AWS_REGION

# All test artifacts live in one scratch dir so this script is
# re-runnable without confusing itself with stale state. We
# don't rm -rf $HOME/anything.
SCRATCH=$(mktemp -d /tmp/tzip-s3-sanity.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT

echo "tzip binary:     $TZIP"
echo "bucket / key:    s3://${BUCKET}/${KEY}"
echo "region:          $AWS_REGION"
echo "scratch dir:     $SCRATCH"
echo

cd "$SCRATCH"

# ── 1. Seed a two-entry archive locally ────────────────────────
echo "[1/4] building local seed archive"
printf 'first\n'  > a.txt
printf 'second\n' > b.txt
"$TZIP" create seed.zip a.txt b.txt -q
ls -la seed.zip

# ── 2. Upload it (NOT via tzip — this is just setup) ───────────
echo
echo "[2/4] uploading seed to s3://${BUCKET}/${KEY}"
if command -v aws >/dev/null 2>&1; then
    aws s3 cp seed.zip "s3://${BUCKET}/${KEY}"
else
    echo "aws CLI not found on PATH" >&2
    echo "Install it (brew install awscli) or upload seed.zip manually to" >&2
    echo "  s3://${BUCKET}/${KEY}" >&2
    echo "then re-run with:   SEED_ALREADY_UPLOADED=1 $0" >&2
    if [[ -z "${SEED_ALREADY_UPLOADED:-}" ]]; then
        exit 1
    fi
fi

# ── 3. The real test: tzip u s3://… new_file ───────────────────
echo
echo "[3/4] tzip u s3://${BUCKET}/${KEY} c.txt   (the issue #68 code path)"
printf 'third from tzip u\n' > c.txt
"$TZIP" u "s3://${BUCKET}/${KEY}" c.txt

# ── 4. Verify the live object reflects the append ──────────────
echo
echo "[4/4] verifying contents"
LS_OUT=$("$TZIP" ls "s3://${BUCKET}/${KEY}")
echo "$LS_OUT"
# `tzip ls` prints a wide table; names land at end-of-line
# after padding. Match on " name$" (preceding whitespace) so
# e.g. "foo/a.txt" doesn't match "a.txt" and vice versa.
for name in a.txt b.txt c.txt; do
    if ! grep -qE "[[:space:]]${name}$" <<< "$LS_OUT"; then
        echo "FAIL: ${name} missing after append" >&2
        exit 1
    fi
done

CAT_OUT=$("$TZIP" cat "s3://${BUCKET}/${KEY}" c.txt)
if [[ "$CAT_OUT" != "third from tzip u" ]]; then
    echo "FAIL: cat c.txt returned: $CAT_OUT" >&2
    exit 1
fi

echo
echo "PASS: tzip u s3:// appended c.txt and all three entries round-trip."
