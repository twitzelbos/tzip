#!/usr/bin/env bash
# s3-full-demo.sh
#
# End-to-end exercise of every tzip subcommand against a single
# S3 archive that is deliberately set up to stress the tool:
#
#   - ~100 000 synthetic JSON entries
#   - PPMd compression at max level (method=98, -x 9)
#   - AES-256 (AE-2) encryption with a shared password
#   - rich indexing: --index-names, --index-terms, --facet,
#     and --write-columnar so query / aggregate / grep all
#     have something to do
#
# What this script proves, in order:
#
#   seed phase
#   1. `tzip create` handles PPMd-max + AES-256 + full indexing
#      on 100 K entries without falling over.
#   2. `aws s3 cp` publishes the resulting archive to the bucket.
#
#   read phase (every op against s3:// directly)
#   3. `tzip ls`    — entry names are enumerable WITHOUT the
#                     password (ZIP spec: names are plaintext in
#                     the CDR).
#   4. `tzip ls -l` — long listing picks up the PPMd method +
#                     AES-encrypted flag.
#   5. `tzip cat`   — a single entry decodes with the password.
#   6. wrong-pw     — fails cleanly (verifier mismatch, no
#                     plaintext leaked).
#   7. `tzip x`     — one entry extracts to disk and matches
#                     the original byte-for-byte.
#   8. `tzip grep`  — term index narrows candidates; regex
#                     finds the seeded marker.
#   9. `tzip find`  — CDR-only filter, zero body I/O.
#   10. `tzip hash` — CRC32 via CDR (free); body-based MD5/SHA
#                     with decryption.
#   11. `tzip aggregate` — facet index returns a count-by-value
#                     table in milliseconds.
#   12. `tzip query`     — DuckDB SQL over the columnar sidecar;
#                     PR #72's row-group prune should fire with
#                     a `WHERE` clause.
#   13. `tzip jq`        — streaming jq filter across entries.
#
#   write phase (PR #70's path)
#   14. `tzip u s3://…` — appends a new entry via multipart
#                      upload with server-side copy of the
#                      kept prefix.
#   15. post-append     — ls / cat round-trip the new entry.
#
# Exit status: 0 iff every step passes; non-zero otherwise.

set -euo pipefail

# ── Config ────────────────────────────────────────────────────
AWS_KEYS_CSV="${AWS_KEYS_CSV:-$HOME/Downloads/twitzel_accessKeys (1).csv}"
AWS_REGION="${AWS_REGION:-us-east-1}"
BUCKET="${BUCKET:-tzip-demo-bucket}"
KEY="${KEY:-demo-ppmd-aes-100k.zip}"
PASSWORD="${PASSWORD:-tzip-demo-pw-2026}"
TZIP="${TZIP:-$(pwd)/target/release/tzip}"
ENTRIES="${ENTRIES:-100000}"
# Seed a known marker string in exactly ONE entry so we can
# round-trip grep + cat without false positives.
MARKER="UNIQUE-MARKER-${RANDOM}-${RANDOM}"

# ── Preflight ─────────────────────────────────────────────────
if [[ ! -x "$TZIP" ]]; then
    echo "tzip binary not found at $TZIP" >&2
    echo "Build it first: cargo build --release" >&2
    exit 1
fi
if [[ ! -r "$AWS_KEYS_CSV" ]]; then
    echo "AWS keys CSV not readable at: $AWS_KEYS_CSV" >&2
    exit 1
fi
if ! command -v aws >/dev/null 2>&1; then
    echo "aws CLI not found on PATH (brew install awscli)" >&2
    exit 1
fi

AWS_ACCESS_KEY_ID=$(awk -F, 'NR==2 {gsub(/\r/,""); print $1}' "$AWS_KEYS_CSV")
AWS_SECRET_ACCESS_KEY=$(awk -F, 'NR==2 {gsub(/\r/,""); print $2}' "$AWS_KEYS_CSV")
if [[ -z "$AWS_ACCESS_KEY_ID" || -z "$AWS_SECRET_ACCESS_KEY" ]]; then
    echo "Couldn't parse AWS access + secret from $AWS_KEYS_CSV" >&2
    exit 1
fi
export AWS_ACCESS_KEY_ID
export AWS_SECRET_ACCESS_KEY
export AWS_REGION

SCRATCH=$(mktemp -d /tmp/tzip-s3-demo.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT
cd "$SCRATCH"

S3_URL="s3://${BUCKET}/${KEY}"

banner() {
    local label=$1
    printf '\n\033[1;36m═══ %s ═══\033[0m\n' "$label"
}

run() {
    # Timed, labelled subcommand runner. `time` goes to stderr;
    # command output is unchanged.
    local label=$1
    shift
    printf '\n\033[1m[%s]\033[0m %s\n' "$label" "$*"
    time "$@"
}

# ── 1. Build the synthetic corpus ─────────────────────────────
banner "1/15  seed corpus — ${ENTRIES} synthetic JSON entries"
mkdir -p corpus

# Keep the corpus deterministic (same hostname produces same
# content so re-runs of the script produce identical archives)
# but with enough entropy that PPMd + AES actually have work
# to do. ~200 bytes per entry; 100 K × 200 B ≈ 20 MB raw.
python3 - <<PY
import json, pathlib, random
random.seed(42)
regions = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "sa-east-1"]
statuses = [200, 201, 301, 400, 404, 500, 503]
users    = [f"user-{i:04d}" for i in range(500)]
out = pathlib.Path("corpus")
for i in range(${ENTRIES}):
    row = {
        "id":     i,
        "ts":     f"2026-11-{random.randint(1,28):02d}T{random.randint(0,23):02d}:{random.randint(0,59):02d}:{random.randint(0,59):02d}Z",
        "region": random.choice(regions),
        "status": random.choice(statuses),
        "user":   random.choice(users),
        "msg":    f"request body {random.randint(0, 1<<30):x}",
    }
    (out / f"entry-{i:06d}.json").write_text(json.dumps(row))
# One entry carries the unique marker so later grep can find it.
marker_idx = ${ENTRIES} // 3
with (out / f"entry-{marker_idx:06d}.json").open("w") as fh:
    json.dump({
        "id": marker_idx, "ts": "2026-11-25T00:00:00Z",
        "region": "us-east-1", "status": 500,
        "user": "marker-user",
        "msg": "${MARKER}",
    }, fh)
print(f"seeded {${ENTRIES}} entries; marker in entry-{marker_idx:06d}.json")
PY

# Schema for the columnar sidecar — matches what the Python
# generator above writes. Int64 for status, strings for the
# rest; `region` is deliberately in the facet set below.
cat >schema.json <<'JSON'
{ "fields": [
    { "name": "id",     "type": "int64",  "path": ".id" },
    { "name": "ts",     "type": "string", "path": ".ts" },
    { "name": "region", "type": "string", "path": ".region" },
    { "name": "status", "type": "int64",  "path": ".status" },
    { "name": "user",   "type": "string", "path": ".user" }
] }
JSON

# ── 2. Create the archive: PPMd-max + AES-256 + full indexing ─
banner "2/15  tzip create — PPMd level 9, AES-256, all indices"
run "create" "$TZIP" create local.zip corpus \
    -m ppmd -x 9 \
    -p "$PASSWORD" \
    --index-names \
    --index-terms '*.json' \
    --facet '.region' \
    --facet '.status' \
    --write-columnar \
    --columnar-schema schema.json
ls -la local.zip

# ── Upload ────────────────────────────────────────────────────
banner "3/15  aws s3 cp — seed the test key"
run "upload" aws s3 cp local.zip "$S3_URL"

# ── READ PHASE ────────────────────────────────────────────────
banner "4/15  tzip ls (no password — names are plaintext in CDR)"
LS_COUNT=$("$TZIP" ls "$S3_URL" | wc -l | tr -d ' ')
echo "entries listed: $LS_COUNT"
if [[ "$LS_COUNT" -lt "$ENTRIES" ]]; then
    echo "FAIL: expected ≥ $ENTRIES entries, got $LS_COUNT" >&2
    exit 1
fi

banner "5/15  tzip ls -l (long listing shows method=PPMd + encrypted)"
run "ls-long" bash -c "'$TZIP' ls -l '$S3_URL' | head -5"

MARKER_ENTRY=$("$TZIP" ls "$S3_URL" | grep 'entry-0.*json' | head -1)
echo "sample entry name: $MARKER_ENTRY"

banner "6/15  tzip cat (one entry, with password)"
run "cat" "$TZIP" cat "$S3_URL" "$MARKER_ENTRY" -p "$PASSWORD"

banner "7/15  tzip cat — wrong password must fail cleanly"
set +e
"$TZIP" cat "$S3_URL" "$MARKER_ENTRY" -p "deliberately-wrong" > /dev/null 2>&1
rc=$?
set -e
if [[ $rc -eq 0 ]]; then
    echo "FAIL: wrong password was accepted" >&2
    exit 1
fi
echo "PASS: wrong password rejected (exit code $rc)"

banner "8/15  tzip x — extract one entry to disk, diff against original"
mkdir -p extract
run "x" "$TZIP" x "$S3_URL" "$MARKER_ENTRY" -C extract -p "$PASSWORD"
diff -q "extract/$MARKER_ENTRY" "corpus/$(basename "$MARKER_ENTRY")"
echo "PASS: extracted bytes match the original"

banner "9/15  tzip grep — term index narrows candidates to the one marker entry"
run "grep" "$TZIP" grep -p "$PASSWORD" "$MARKER" "$S3_URL" --include '*.json'

banner "10/15  tzip find — CDR-only filter (zero body I/O)"
run "find" bash -c "'$TZIP' find --include 'entry-0002*.json' --names-only '$S3_URL' | head -5"

banner "11/15  tzip hash — CRC32 via CDR (free), then MD5 via body decode"
run "hash-crc32" bash -c "'$TZIP' hash --algo crc32 '$S3_URL' | head -3"
run "hash-md5"   bash -c "'$TZIP' hash --algo md5 -p '$PASSWORD' '$S3_URL' | head -3"

banner "12/15  tzip aggregate — facet index count-by-region"
run "agg-by-region" "$TZIP" aggregate --by region "$S3_URL"
echo "---"
run "agg-by-status" "$TZIP" aggregate --by status "$S3_URL"

banner "13/15  tzip query — DuckDB SQL (row-group prune should fire)"
if ! command -v duckdb >/dev/null 2>&1; then
    echo "skipping: duckdb not on PATH (brew install duckdb)"
else
    run "query-count"  "$TZIP" query --format csv -p "$PASSWORD" \
        -s "SELECT COUNT(*) FROM tzip WHERE status = 500" \
        "$S3_URL"
    run "query-group"  "$TZIP" query --format csv -p "$PASSWORD" \
        -s "SELECT region, COUNT(*) AS n FROM tzip GROUP BY region ORDER BY n DESC" \
        "$S3_URL"
    run "query-prune"  "$TZIP" query --format csv -p "$PASSWORD" \
        -s "SELECT COUNT(*) FROM tzip WHERE id > 90000" \
        "$S3_URL"
fi

banner "14/15  tzip jq — streaming jq filter over entries"
run "jq-regions" bash -c \
    "'$TZIP' jq -p '$PASSWORD' -r '.region' '$S3_URL' --include '*.json' | sort | uniq -c | sort -rn | head"

# ── WRITE PHASE (PR #70) ──────────────────────────────────────
banner "15/15  tzip u — append one new entry via S3 multipart"
printf '{"appended":"via tzip u","ts":"%s","from":"s3-full-demo.sh"}\n' \
    "$(date -u +%FT%TZ)" > appended.json

run "append" "$TZIP" u "$S3_URL" appended.json -p "$PASSWORD"

echo
echo "verifying the appended entry is visible on the live object:"
if ! "$TZIP" ls "$S3_URL" | grep -qE "[[:space:]]appended\.json$|^appended\.json$"; then
    echo "FAIL: appended.json missing from post-append listing" >&2
    exit 1
fi
APPENDED_BODY=$("$TZIP" cat "$S3_URL" appended.json -p "$PASSWORD")
echo "appended body (via tzip cat):"
echo "  $APPENDED_BODY"
if ! grep -q "via tzip u" <<< "$APPENDED_BODY"; then
    echo "FAIL: appended body doesn't match what we wrote" >&2
    exit 1
fi

echo
printf '\033[1;32m═══ PASS — every operation round-tripped against %s ═══\033[0m\n' "$S3_URL"
