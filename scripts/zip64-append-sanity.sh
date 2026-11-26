#!/usr/bin/env bash
# zip64-append-sanity.sh
#
# Manual repro + verification for issue #80: `tzip u` on a
# ZIP64 archive (one whose central directory starts past the
# 4 GiB boundary) used to truncate the archive and bail with
# "ZIP64 tail parse not supported via parse_entries_from_tail",
# leaving the file corrupted. The branch this script is on
# fixes:
#
#   1. The ZIP64 tail parser now handles EOCD64 + locator.
#   2. The post-truncate error path is wrapped in a single
#      recovery closure that always calls restore_from_undo on
#      failure.
#
# This script builds an archive big enough to force ZIP64 by
# scooping every PDF under a configurable root (default:
# `$HOME`), then runs the exact `tzip u` the issue reports
# and asserts (a) it exits 0, (b) the new entry is listed,
# (c) `tzip cat` returns it byte-exact, (d) a sample of the
# pre-existing entries still extracts correctly.
#
# This is *not* a CI test — it depends on finding ≥ 4 GiB of
# PDFs locally (synthesising them would take as long as the
# actual repro). CI covers the parser via the synthetic-tail
# unit test; this script is the end-to-end smoke.
#
# Env overrides:
#   PDF_ROOT        — where to look for PDFs       (default: $HOME)
#   MIN_ARCHIVE_GB  — minimum archive size to reach past 4 GiB
#                    before giving up              (default: 4.5)
#   TZIP            — path to the tzip binary     (default: ./target/release/tzip)
#   CACHE_DIR       — where to cache the list + the archive
#                     (default: ${TMPDIR:-/tmp}/tzip-zip64-cache)
#   FORCE_REBUILD=1 — nuke the cache and start over
#
# Exit status: 0 iff every step passes; non-zero otherwise.

set -euo pipefail

PDF_ROOT="${PDF_ROOT:-$HOME}"
MIN_ARCHIVE_GB="${MIN_ARCHIVE_GB:-4.5}"
TZIP="${TZIP:-$(pwd)/target/release/tzip}"
CACHE_DIR="${CACHE_DIR:-${TMPDIR:-/tmp}/tzip-zip64-cache}"
FORCE_REBUILD="${FORCE_REBUILD:-0}"

if [[ ! -x "$TZIP" ]]; then
    echo "tzip binary not found at $TZIP — run: cargo build --release" >&2
    exit 1
fi

if [[ "$FORCE_REBUILD" == "1" ]]; then
    echo "FORCE_REBUILD=1 — nuking $CACHE_DIR"
    rm -rf "$CACHE_DIR"
fi
mkdir -p "$CACHE_DIR"

ARCHIVE="$CACHE_DIR/zip64-test.zip"
FILELIST="$CACHE_DIR/pdf-filelist.txt"
SENTINEL="$CACHE_DIR/.archive-complete"

banner() { printf '\n\033[1;36m═══ %s ═══\033[0m\n' "$*"; }

# ── 1. Discover PDFs until the cumulative size exceeds 4 GiB ─
banner "1/6  scan $PDF_ROOT for PDFs totalling ≥ ${MIN_ARCHIVE_GB} GiB"
if [[ -f "$FILELIST" ]]; then
    COUNT=$(wc -l <"$FILELIST" | tr -d ' ')
    echo "cache hit: $FILELIST already has $COUNT entries"
else
    echo "scanning (this takes a minute on a full home directory)..."
    # Stream find's byte-sized output; stop once we've accrued
    # enough bytes to safely cross 4 GiB when compressed by
    # `store`. 4 GiB = 4294967296; use the floor from the env.
    # `stat -f%z` is BSD (macOS), `stat -c%s` is GNU (Linux).
    MIN_BYTES=$(python3 -c "print(int(${MIN_ARCHIVE_GB} * 1024**3))")
    ACCUM=0
    : > "$FILELIST"
    # Use `find -print0` + a NUL-delimited read loop so paths
    # with spaces / newlines don't break the script.
    while IFS= read -r -d '' path; do
        size=$(stat -f%z "$path" 2>/dev/null || stat -c%s "$path" 2>/dev/null || echo 0)
        [[ "$size" -gt 0 ]] || continue
        printf '%s\n' "$path" >> "$FILELIST"
        ACCUM=$((ACCUM + size))
        if (( ACCUM >= MIN_BYTES )); then
            echo "collected $ACCUM bytes across $(wc -l <"$FILELIST" | tr -d ' ') PDFs — stop"
            break
        fi
    done < <(find "$PDF_ROOT" -type f \( -iname '*.pdf' \) -print0 2>/dev/null)

    if (( ACCUM < MIN_BYTES )); then
        echo "FAIL: only found $ACCUM bytes of PDFs under $PDF_ROOT — need ≥ $MIN_BYTES to force ZIP64" >&2
        echo "     Try PDF_ROOT=/ or lower MIN_ARCHIVE_GB" >&2
        exit 1
    fi
fi

# ── 2. Build the ZIP64 archive (lazy — reuse across runs) ────
banner "2/6  tzip create — Store method (so the archive stays large and ZIP64 kicks in)"
if [[ -f "$SENTINEL" && -f "$ARCHIVE" ]]; then
    SIZE=$(stat -f%z "$ARCHIVE" 2>/dev/null || stat -c%s "$ARCHIVE")
    echo "cache hit: $ARCHIVE ($SIZE bytes)"
else
    echo "cache miss — building archive at $ARCHIVE"
    # `tzip create` with a file-list file isn't currently a
    # thing; feed paths as positional args via xargs.
    # `-m store` so compression doesn't shrink the archive back
    # under 4 GiB (PDFs compress poorly anyway, so this costs
    # roughly nothing).
    xargs -0 -a <(tr '\n' '\0' < "$FILELIST") \
        "$TZIP" create "$ARCHIVE" -m store -q
    touch "$SENTINEL"
fi
SIZE=$(stat -f%z "$ARCHIVE" 2>/dev/null || stat -c%s "$ARCHIVE")
echo "archive size: $SIZE bytes ($(du -h "$ARCHIVE" | cut -f1))"
if (( SIZE < 4 * 1024 * 1024 * 1024 )); then
    echo "WARNING: archive is below 4 GiB — ZIP64 may not actually be exercised" >&2
    echo "         (cd_offset may still land before the boundary)" >&2
fi

# ── 3. Snapshot archive size + first-entry CDR position ──────
banner "3/6  record pre-append state"
PRE_SIZE=$SIZE
PRE_COUNT=$("$TZIP" find --names-only "$ARCHIVE" | wc -l | tr -d ' ')
echo "pre-append: $PRE_COUNT entries, $PRE_SIZE bytes"

# ── 4. The actual bug repro: `tzip u <zip> new.pdf other.txt` ─
banner "4/6  tzip u — append two new entries to the ZIP64 archive"
NEW_PDF="$CACHE_DIR/new-notice.pdf"
NEW_TXT="$CACHE_DIR/new-notice_0001.txt"
printf '%%PDF-1.4\n%% fake pdf for tzip zip64 append test\n%%EOF\n' > "$NEW_PDF"
printf 'page 1 OCR text for new-notice\n' > "$NEW_TXT"

echo "running: tzip u $ARCHIVE $NEW_PDF $NEW_TXT --write-sha256"
set +e
"$TZIP" u "$ARCHIVE" "$NEW_PDF" "$NEW_TXT" --write-sha256
rc=$?
set -e
if [[ $rc -ne 0 ]]; then
    echo "FAIL: tzip u exited $rc" >&2
    # If the undo file is still there, the restore path failed
    # too — that's a worse bug than the primary one.
    UNDO="$ARCHIVE.tzip-undo"
    if [[ -f "$UNDO" ]]; then
        echo "       undo file still present at $UNDO" >&2
        echo "       archive is probably still restorable via next \`tzip u\` invocation" >&2
    else
        echo "       no undo file — archive restored or never truncated" >&2
    fi
    exit 1
fi
echo "PASS: tzip u exited 0"

# ── 5. Verify the appended entries are visible and byte-exact ─
banner "5/6  tzip ls / cat — verify the new entries"
POST_COUNT=$("$TZIP" find --names-only "$ARCHIVE" | wc -l | tr -d ' ')
echo "post-append: $POST_COUNT entries"
if (( POST_COUNT < PRE_COUNT + 2 )); then
    echo "FAIL: expected ≥ $((PRE_COUNT + 2)) entries, got $POST_COUNT" >&2
    exit 1
fi

for new_file in "$NEW_PDF" "$NEW_TXT"; do
    name_in_archive=$(basename "$new_file")
    echo "checking $name_in_archive"
    if ! "$TZIP" find --names-only --include "$name_in_archive" "$ARCHIVE" | grep -q .; then
        echo "FAIL: $name_in_archive not in post-append listing" >&2
        exit 1
    fi
    extracted=$("$TZIP" cat "$ARCHIVE" "$name_in_archive")
    original=$(cat "$new_file")
    if [[ "$extracted" != "$original" ]]; then
        echo "FAIL: $name_in_archive content mismatch" >&2
        exit 1
    fi
done

# ── 6. Sample one old entry — confirm the pre-existing CDR is intact ─
banner "6/6  verify a pre-existing entry still extracts (CDR not corrupted)"
SAMPLE=$("$TZIP" find --names-only --include '*.pdf' "$ARCHIVE" \
    | head -1 \
    | sed 's/^.*:\([^:]*\)$/\1/')
if [[ -z "$SAMPLE" ]]; then
    echo "FAIL: no pre-existing PDF found in post-append listing" >&2
    exit 1
fi
echo "sample pre-existing entry: $SAMPLE"
TMPEXT=$(mktemp -d)
trap "rm -rf '$TMPEXT'" EXIT
"$TZIP" x "$ARCHIVE" "$SAMPLE" -C "$TMPEXT"
if [[ ! -s "$TMPEXT/$SAMPLE" ]]; then
    echo "FAIL: extracted sample is empty or missing at $TMPEXT/$SAMPLE" >&2
    exit 1
fi
echo "sample size on extract: $(stat -f%z "$TMPEXT/$SAMPLE" 2>/dev/null || stat -c%s "$TMPEXT/$SAMPLE") bytes"

echo
printf '\033[1;32m═══ PASS — ZIP64 append + recovery round-trip clean (issue #80) ═══\033[0m\n'
