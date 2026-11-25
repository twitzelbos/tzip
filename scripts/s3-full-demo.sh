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
#
# Re-runnability:
#   - The ~100 K-entry corpus and the PPMd-max archive built on
#     top of it are expensive. Both are cached under
#     `${TMPDIR:-/tmp}/tzip-s3-demo-cache-${ENTRIES}/` and reused
#     verbatim on subsequent runs. Pass `FORCE_REBUILD=1` to
#     wipe the cache, or set CACHE_DIR to an explicit path.
#   - The S3 upload is skipped when the live object already
#     matches the cached archive by size (and re-done otherwise
#     — e.g. after the previous run's step 15 mutated the live
#     object with an `appended.json`).
#   - Step 15 (`tzip u`) is itself idempotent: if the live
#     object already carries `appended.json`, the append is
#     skipped and the verification still runs.

set -euo pipefail

# ── Config ────────────────────────────────────────────────────
AWS_KEYS_CSV="${AWS_KEYS_CSV:-$HOME/Downloads/twitzel_accessKeys (1).csv}"
AWS_REGION="${AWS_REGION:-us-east-1}"
BUCKET="${BUCKET:-tzip-demo-bucket}"
PASSWORD="${PASSWORD:-tzip-demo-pw-2026}"
TZIP="${TZIP:-$(pwd)/target/release/tzip}"
ENTRIES="${ENTRIES:-100000}"
KEY="${KEY:-demo-ppmd-aes-${ENTRIES}.zip}"
# A FIXED marker string so a cached corpus (see CACHE_DIR
# below) carries it reproducibly across runs. Grep asserts
# this exact string, so changing it requires invalidating the
# cache or setting FORCE_REBUILD=1.
MARKER="${MARKER:-UNIQUE-MARKER-FOR-TZIP-GREP-TEST}"
# Cached corpus + prebuilt archive live outside the per-run
# scratch dir so re-runs skip the 100 K-file + PPMd-max
# regeneration (which is the slow part by far). Keyed on
# ENTRIES so bumping the size naturally invalidates the
# cache. FORCE_REBUILD=1 forces a rebuild.
CACHE_DIR="${CACHE_DIR:-${TMPDIR:-/tmp}/tzip-s3-demo-cache-${ENTRIES}}"
FORCE_REBUILD="${FORCE_REBUILD:-0}"

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

# Per-run scratch dir — only holds transient artifacts (extract
# target, appended.json, etc.). The expensive-to-build corpus
# + archive + schema live in CACHE_DIR and survive across runs.
SCRATCH=$(mktemp -d /tmp/tzip-s3-demo-run.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT
cd "$SCRATCH"

S3_URL="s3://${BUCKET}/${KEY}"
CACHED_ARCHIVE="$CACHE_DIR/local.zip"
CACHED_CORPUS="$CACHE_DIR/corpus"
CACHED_SCHEMA="$CACHE_DIR/schema.json"
# Sentinel file; its presence means "corpus generation
# finished successfully for this ENTRIES size." Avoids a
# half-generated corpus being mistaken for a cache hit after
# a Ctrl-C during the Python loop.
CACHED_SENTINEL="$CACHE_DIR/.corpus-complete"

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

# ── 1. Seed the synthetic corpus (lazy — reused across runs) ──
banner "1/15  seed corpus — ${ENTRIES} synthetic JSON entries"

if [[ "$FORCE_REBUILD" == "1" ]]; then
    echo "FORCE_REBUILD=1 — nuking cache at $CACHE_DIR"
    rm -rf "$CACHE_DIR"
fi

mkdir -p "$CACHE_DIR"

if [[ -f "$CACHED_SENTINEL" ]]; then
    N=$(find "$CACHED_CORPUS" -name 'entry-*.json' -maxdepth 1 | wc -l | tr -d ' ')
    echo "cache hit: $N entries at $CACHED_CORPUS"
    if [[ "$N" -ne "$ENTRIES" ]]; then
        echo "  cache entry-count mismatch ($N ≠ $ENTRIES) — rebuilding"
        rm -rf "$CACHED_CORPUS" "$CACHED_SENTINEL" "$CACHED_ARCHIVE"
    fi
fi

if [[ ! -f "$CACHED_SENTINEL" ]]; then
    echo "cache miss: regenerating $ENTRIES entries at $CACHED_CORPUS"
    mkdir -p "$CACHED_CORPUS"
    # Deterministic seed → same bytes on every regen for a
    # given ENTRIES, which keeps the archive content-addressed
    # across runs. The MARKER lands in a fixed slot so grep
    # asserts the same value every time.
    python3 - "$CACHED_CORPUS" "$ENTRIES" "$MARKER" <<'PY'
import json, pathlib, random, sys
out, entries, marker = pathlib.Path(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
random.seed(42)
regions  = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "sa-east-1"]
statuses = [200, 201, 301, 400, 404, 500, 503]
users    = [f"user-{i:04d}" for i in range(500)]
for i in range(entries):
    (out / f"entry-{i:06d}.json").write_text(json.dumps({
        "id":     i,
        "ts":     f"2026-11-{random.randint(1,28):02d}T{random.randint(0,23):02d}:{random.randint(0,59):02d}:{random.randint(0,59):02d}Z",
        "region": random.choice(regions),
        "status": random.choice(statuses),
        "user":   random.choice(users),
        "msg":    f"request body {random.randint(0, 1<<30):x}",
    }))
# Fixed marker slot so grep's assertion is cache-stable.
marker_idx = entries // 3
(out / f"entry-{marker_idx:06d}.json").write_text(json.dumps({
    "id": marker_idx, "ts": "2026-11-25T00:00:00Z",
    "region": "us-east-1", "status": 500,
    "user": "marker-user",
    "msg": marker,
}))
print(f"seeded {entries} entries; marker in entry-{marker_idx:06d}.json")
PY
    touch "$CACHED_SENTINEL"
fi

# Schema for the columnar sidecar; cheap to regenerate but
# lives in the cache dir so the whole seed set is one place.
cat >"$CACHED_SCHEMA" <<'JSON'
{ "fields": [
    { "name": "id",     "type": "int64",  "path": ".id" },
    { "name": "ts",     "type": "string", "path": ".ts" },
    { "name": "region", "type": "string", "path": ".region" },
    { "name": "status", "type": "int64",  "path": ".status" },
    { "name": "user",   "type": "string", "path": ".user" }
] }
JSON

# ── 2. Build the archive (lazy — reused across runs) ──────────
banner "2/15  tzip create — PPMd level 9, AES-256, all indices"
if [[ -f "$CACHED_ARCHIVE" ]]; then
    echo "cache hit: $CACHED_ARCHIVE ($(du -h "$CACHED_ARCHIVE" | cut -f1))"
else
    echo "cache miss: building archive from $CACHED_CORPUS"
    # `tzip create` wants input paths relative to its cwd —
    # jump into CACHE_DIR so `corpus` resolves correctly, then
    # jump back.
    (
        cd "$CACHE_DIR"
        "$TZIP" create local.zip corpus \
            -m ppmd -x 9 \
            -p "$PASSWORD" \
            --index-names \
            --index-terms '*.json' \
            --facet '.region' \
            --facet '.status' \
            --write-columnar \
            --columnar-schema schema.json
    )
fi
ls -la "$CACHED_ARCHIVE"

# ── Upload (lazy — reuse an already-published object) ─────────
banner "3/15  aws s3 cp — publish to the test key (if missing)"
if aws s3api head-object --bucket "$BUCKET" --key "$KEY" >/dev/null 2>&1; then
    REMOTE_SIZE=$(aws s3api head-object --bucket "$BUCKET" --key "$KEY" --query 'ContentLength' --output text)
    LOCAL_SIZE=$(stat -f%z "$CACHED_ARCHIVE" 2>/dev/null || stat -c%s "$CACHED_ARCHIVE")
    if [[ "$REMOTE_SIZE" == "$LOCAL_SIZE" ]]; then
        echo "remote object s3://${BUCKET}/${KEY} already matches local size ($LOCAL_SIZE) — skipping upload"
    else
        echo "remote object size $REMOTE_SIZE ≠ local $LOCAL_SIZE — re-uploading"
        run "upload" aws s3 cp "$CACHED_ARCHIVE" "$S3_URL"
    fi
else
    run "upload" aws s3 cp "$CACHED_ARCHIVE" "$S3_URL"
fi

# ── READ PHASE ────────────────────────────────────────────────
banner "4/15  tzip ls (no password — names are plaintext in CDR)"
# `tzip ls` prints a header + separator + rows + totals table,
# so counting lines overcounts. Use `tzip find --names-only`
# for a clean "one entry name per line" stream instead.
LS_COUNT=$("$TZIP" find --names-only "$S3_URL" | wc -l | tr -d ' ')
echo "entries counted (via find --names-only): $LS_COUNT"
if [[ "$LS_COUNT" -lt "$ENTRIES" ]]; then
    echo "FAIL: expected ≥ $ENTRIES entries, got $LS_COUNT" >&2
    exit 1
fi

banner "5/15  tzip ls — the default long listing already shows method / mtime / name"
# `tzip ls` has no `-l` flag — the default output IS the long
# form. Older versions of this script passed `-l`, which errors
# out.
run "ls-head" bash -c "'$TZIP' ls '$S3_URL' | head -8"

# Pick one `entry-NNNNNN.json` for the per-entry ops that
# follow. `tzip find --names-only` emits `<archive>:<entry>`
# per line; strip the archive prefix with the last-colon
# split so S3 URLs' own colon doesn'\''t confuse it.
MARKER_ENTRY=$(
    "$TZIP" find --names-only --include 'corpus/entry-*.json' "$S3_URL" \
    | head -1 \
    | sed 's/^.*:\([^:]*\)$/\1/'
)
if [[ -z "$MARKER_ENTRY" ]]; then
    echo "FAIL: couldn'\''t locate a corpus/entry-*.json in the archive" >&2
    exit 1
fi
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
# The original lives in the cache dir, NOT the per-run scratch;
# `$MARKER_ENTRY` is `corpus/entry-NNNNNN.json`, so the
# original is `$CACHED_CORPUS/<basename>` (CACHED_CORPUS is
# already `.../corpus`).
diff -q "extract/$MARKER_ENTRY" "$CACHED_CORPUS/$(basename "$MARKER_ENTRY")"
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

# Re-runnable: the previous script invocation may have left
# `appended.json` on the live object. tzip u's pure-add scope
# (issue #68 v1) rejects collisions, so bail out of the append
# call itself but keep the verification. If the previous run
# also re-uploaded the clean archive in step 3 (size-mismatch
# path) then the live object no longer has `appended.json` and
# we do a fresh append here.
if "$TZIP" ls "$S3_URL" | grep -qE "[[:space:]]appended\.json$|^appended\.json$"; then
    echo "live object already has appended.json (left by a prior run) — skipping append"
else
    printf '{"appended":"via tzip u","ts":"%s","from":"s3-full-demo.sh"}\n' \
        "$(date -u +%FT%TZ)" > appended.json
    run "append" "$TZIP" u "$S3_URL" appended.json -p "$PASSWORD"
fi

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
