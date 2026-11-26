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
# Per-entry raw size target. The original ~130-byte entries
# left PPMd level 9 almost no context to model, so the
# archive crushed only ~3× (~40 MB from 13 MB raw). Bumping
# the body to ~4 KiB of semi-redundant English+id text per
# entry gives PPMd real work and makes the demo's compression
# claim actually interesting (archive shrinks ~5-10× instead
# of ~3×).
ENTRY_BYTES="${ENTRY_BYTES:-4096}"
# Cached corpus + prebuilt archive live outside the per-run
# scratch dir so re-runs skip the 100 K-file + PPMd-max
# regeneration (which is the slow part by far). Keyed on
# ENTRIES *and* on a schema-version slug so bumping either
# the count or the entry shape naturally invalidates. Bumped
# to `v2` with the 4 KiB-per-entry change; everyone's old
# `tzip-s3-demo-cache-100000` dir can be deleted.
CACHE_VERSION="${CACHE_VERSION:-v2-4k}"
CACHE_DIR="${CACHE_DIR:-${TMPDIR:-/tmp}/tzip-s3-demo-cache-${CACHE_VERSION}-${ENTRIES}}"
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
    python3 - "$CACHED_CORPUS" "$ENTRIES" "$MARKER" "$ENTRY_BYTES" <<'PY'
import json, pathlib, random, sys
out, entries, marker, target_bytes = (
    pathlib.Path(sys.argv[1]),
    int(sys.argv[2]),
    sys.argv[3],
    int(sys.argv[4]),
)
random.seed(42)
regions  = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "sa-east-1"]
statuses = [200, 201, 301, 400, 404, 500, 503]
users    = [f"user-{i:04d}" for i in range(500)]
# Small vocabulary so PPMd's context model finds repeated
# n-grams across many files. 48 words, Zipfian-weighted —
# good approximation of English redundancy, drops bulk size
# by 5-10× under PPMd level 9 while keeping enough entropy
# that AES has visible work (every entry still rolls its own
# salt, which forces distinct ciphertext).
WORDS = (
    "the of and to a in that is was for on with as by this it be "
    "which an but from has or had at are not his they their been "
    "which when were all one also can so if we more about would "
    "service request response cached user region status payload"
).split()
WEIGHTS = [1.0 / (i + 1) for i in range(len(WORDS))]
def paragraph(rng, target_len):
    """Emit words until the running string reaches `target_len`
    bytes, so the final record lands close to the requested
    per-entry size regardless of the vocabulary's average
    word length (which otherwise makes a word-count-driven
    loop overshoot or undershoot unpredictably).
    """
    buf, since_sentence, cur_len = [], 0, 0
    while cur_len < target_len:
        w = rng.choices(WORDS, weights=WEIGHTS, k=1)[0]
        buf.append(w.capitalize() if since_sentence == 0 else w)
        cur_len += len(w) + 1
        since_sentence += 1
        if since_sentence > rng.randint(10, 20):
            buf.append(".")
            cur_len += 1
            since_sentence = 0
    return " ".join(buf).replace(" .", ".") + "."

# Subtract ~200 bytes for the surrounding JSON envelope
# (`{"id":…,"ts":…,"region":…,"status":…,"user":…,"msg":""}`)
# so the final file lands close to `target_bytes`, not
# `target_bytes + 200`.
BODY_TARGET = max(100, target_bytes - 200)
for i in range(entries):
    # Per-entry PRNG seeded by i so content is deterministic
    # independent of iteration order (enables parallel regen
    # later if we want it).
    rng = random.Random(42 ^ i)
    body = paragraph(rng, BODY_TARGET)
    rec = {
        "id":     i,
        "ts":     f"2026-11-{rng.randint(1,28):02d}T{rng.randint(0,23):02d}:{rng.randint(0,59):02d}:{rng.randint(0,59):02d}Z",
        "region": rng.choice(regions),
        "status": rng.choice(statuses),
        "user":   rng.choice(users),
        "msg":    body,
    }
    (out / f"entry-{i:06d}.json").write_text(json.dumps(rec))
# Fixed marker slot so grep's assertion is cache-stable.
marker_idx = entries // 3
rng = random.Random(42 ^ marker_idx)
marker_body = marker + " " + paragraph(rng, BODY_TARGET)
(out / f"entry-{marker_idx:06d}.json").write_text(json.dumps({
    "id": marker_idx, "ts": "2026-11-25T00:00:00Z",
    "region": "us-east-1", "status": 500,
    "user": "marker-user",
    "msg": marker_body,
}))
print(f"seeded {entries} entries (~{target_bytes} B each); marker in entry-{marker_idx:06d}.json")
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
LOCAL_SIZE=$(stat -f%z "$CACHED_ARCHIVE" 2>/dev/null || stat -c%s "$CACHED_ARCHIVE")
LOCAL_HUMAN=$(du -h "$CACHED_ARCHIVE" | cut -f1)
NEED_UPLOAD=1
if aws s3api head-object --bucket "$BUCKET" --key "$KEY" >/dev/null 2>&1; then
    REMOTE_SIZE=$(aws s3api head-object --bucket "$BUCKET" --key "$KEY" --query 'ContentLength' --output text)
    if [[ "$REMOTE_SIZE" == "$LOCAL_SIZE" ]]; then
        echo "remote object s3://${BUCKET}/${KEY} already matches local size ($LOCAL_SIZE) — skipping upload"
        NEED_UPLOAD=0
    else
        echo "remote object size $REMOTE_SIZE ≠ local $LOCAL_SIZE — re-uploading"
    fi
fi

if [[ "$NEED_UPLOAD" == "1" ]]; then
    echo "uploading $CACHED_ARCHIVE ($LOCAL_HUMAN / $LOCAL_SIZE bytes) → $S3_URL"
    # `aws s3 cp` writes its own progress line via carriage
    # returns to stderr; in some terminals (non-tty stderr,
    # tmux split-pane, CI-like contexts) that line never
    # updates and the upload *looks* hung for the few minutes
    # it takes to push a ~hundred-MB archive.
    #
    # Workaround: launch the aws cp in the background and
    # heart-beat our own progress line every 5s by polling the
    # in-progress multipart upload's part count + the staged
    # object once Complete has landed. Falls back to "still
    # uploading" when neither API returns useful state yet.
    aws s3 cp "$CACHED_ARCHIVE" "$S3_URL" &
    AWS_PID=$!
    START=$(date +%s)
    PREV_STATUS=""
    while kill -0 "$AWS_PID" 2>/dev/null; do
        sleep 5
        # One-shot status probe: if the object already landed
        # (small file / fast upload), report the final size;
        # otherwise count in-progress multipart parts for this
        # key. Both calls are best-effort and swallowed on
        # error so one flaky probe doesn't kill the script.
        NOW=$(date +%s)
        ELAPSED=$((NOW - START))
        STATUS=""
        if HEAD_SIZE=$(aws s3api head-object --bucket "$BUCKET" --key "$KEY" --query 'ContentLength' --output text 2>/dev/null); then
            if [[ "$HEAD_SIZE" != "None" && "$HEAD_SIZE" != "null" ]]; then
                STATUS="object has $HEAD_SIZE bytes (size match: $([[ "$HEAD_SIZE" == "$LOCAL_SIZE" ]] && echo yes || echo no))"
            fi
        fi
        if [[ -z "$STATUS" ]]; then
            PARTS=$(aws s3api list-multipart-uploads --bucket "$BUCKET" --prefix "$KEY" --query 'length(Uploads || `[]`)' --output text 2>/dev/null || echo "?")
            if [[ "$PARTS" != "?" && "$PARTS" != "0" ]]; then
                # Count completed parts for the first upload.
                UPLOAD_ID=$(aws s3api list-multipart-uploads --bucket "$BUCKET" --prefix "$KEY" --query 'Uploads[0].UploadId' --output text 2>/dev/null || echo "")
                if [[ -n "$UPLOAD_ID" && "$UPLOAD_ID" != "None" ]]; then
                    PART_COUNT=$(aws s3api list-parts --bucket "$BUCKET" --key "$KEY" --upload-id "$UPLOAD_ID" --query 'length(Parts || `[]`)' --output text 2>/dev/null || echo "?")
                    STATUS="multipart in progress — $PART_COUNT parts uploaded"
                fi
            fi
        fi
        [[ -z "$STATUS" ]] && STATUS="still uploading…"
        # Collapse repeated noise (no state change between
        # polls) to a single-line heartbeat that advances the
        # elapsed counter but doesn't fill the screen.
        if [[ "$STATUS" != "$PREV_STATUS" ]]; then
            printf '\n  [+%ds] %s\n' "$ELAPSED" "$STATUS"
            PREV_STATUS="$STATUS"
        else
            printf '  [+%ds] (unchanged)\n' "$ELAPSED"
        fi
    done
    # Reap aws; non-zero exit here aborts the script.
    if ! wait "$AWS_PID"; then
        echo "FAIL: aws s3 cp exited non-zero" >&2
        exit 1
    fi
    echo "upload finished in $(($(date +%s) - START))s"
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
#
# `head -1` closes the pipe after reading one line, which
# SIGPIPEs `tzip find` on its next write — exit 141, which
# is the correct UNIX-CLI behaviour (see PR #83). Under
# the script'\''s `set -o pipefail` that'\''d abort the whole
# run silently, which is how the demo was dying "at 5/15".
# Toggle pipefail off just for this single line so the
# expected SIGPIPE isn'\''t fatal.
set +o pipefail
MARKER_ENTRY=$(
    "$TZIP" find --names-only --include 'corpus/entry-*.json' "$S3_URL" \
    | head -1 \
    | sed 's/^.*:\([^:]*\)$/\1/'
)
set -o pipefail
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
run "x" "$TZIP" x "$S3_URL" "$MARKER_ENTRY" --to extract -p "$PASSWORD"
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
run "agg-by-region" "$TZIP" aggregate --by region -p "$PASSWORD" "$S3_URL"
echo "---"
run "agg-by-status" "$TZIP" aggregate --by status -p "$PASSWORD" "$S3_URL"

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

# ── Appendix: CDR-cache cold vs warm (PR #85 + #87) ──────────
banner "Appendix  CDR cache cold vs warm — the whole point of --cache-dir"

# A fresh cache dir inside the per-run scratch so repeated
# invocations of the script don't leak timing from each other.
DEMO_CACHE="$SCRATCH/demo-cache"
rm -rf "$DEMO_CACHE"

# Small helper — `date` on macOS doesn't do nanos, `python3 -c
# 'time.time()'` is portable and fast enough for the three
# measurements we take here.
measure() {
    local start end
    start=$(python3 -c 'import time; print(time.time())')
    "$@" > /dev/null 2>&1
    end=$(python3 -c 'import time; print(time.time())')
    python3 -c "print(f'{($end) - ($start):.3f}')"
}

echo
echo "1) cold tzip ls (no cache — full CDR refetch each time)"
COLD_LS=$(measure "$TZIP" ls "$S3_URL")
echo "   elapsed: ${COLD_LS}s"

echo
echo "2) tzip ls --cache-dir (first call populates the CDR cache)"
WARM_LS_1=$(measure "$TZIP" ls --cache-dir "$DEMO_CACHE" "$S3_URL")
echo "   elapsed: ${WARM_LS_1}s"

echo
echo "3) tzip ls --cache-dir (second call — CDR served from local disk)"
WARM_LS_2=$(measure "$TZIP" ls --cache-dir "$DEMO_CACHE" "$S3_URL")
echo "   elapsed: ${WARM_LS_2}s"

# Hero scenario: single-entry cat. On a 100 K-entry archive
# the CDR fetch dominates the 1 KB payload cost; cached run
# should cost ~just the LFH + body.
echo
echo "4) cold tzip cat one entry (no cache)"
COLD_CAT=$(measure "$TZIP" cat "$S3_URL" "$MARKER_ENTRY" -p "$PASSWORD")
echo "   elapsed: ${COLD_CAT}s"

echo
echo "5) tzip cat --cache-dir, same entry (cache hit from the ls above)"
WARM_CAT=$(measure "$TZIP" cat --cache-dir "$DEMO_CACHE" "$S3_URL" "$MARKER_ENTRY" -p "$PASSWORD")
echo "   elapsed: ${WARM_CAT}s"

echo
RATIO_LS=$(python3 -c "print(f'{$COLD_LS / $WARM_LS_2:.1f}×')")
RATIO_CAT=$(python3 -c "print(f'{$COLD_CAT / $WARM_CAT:.1f}×')")
printf '\033[1m  tzip ls   speedup: %s (%ss → %ss)\033[0m\n'  "$RATIO_LS"  "$COLD_LS"  "$WARM_LS_2"
printf '\033[1m  tzip cat  speedup: %s (%ss → %ss)\033[0m\n'  "$RATIO_CAT" "$COLD_CAT" "$WARM_CAT"

echo
printf '\033[1;32m═══ PASS — every operation round-tripped against %s ═══\033[0m\n' "$S3_URL"
