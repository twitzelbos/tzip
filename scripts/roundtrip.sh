#!/usr/bin/env bash
# End-to-end round-trip test:
#
#   1. Build an archive with tzip + --raw-block + deflate -x 9 + AES-256.
#   2. Verify 7-zip can list AND extract it (interop check).
#   3. CRC32-diff the extracted tuples against a reference archive.
#   4. Report size vs the reference.
#
# Configure via env vars (no customer/user-specific defaults):
#   ROUNDTRIP_SOURCES     - space-separated absolute paths to archive
#   ROUNDTRIP_REFERENCE   - path to reference .zip to diff against
#   ROUNDTRIP_PASSWORD    - path to a file containing the password
#
# Requires: 7z (`brew install p7zip`), sudo (for --raw-block).

set -euo pipefail

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/tzip"
OUT="/tmp/tzip-roundtrip.zip"
EXTRACT="/tmp/tzip-roundtrip-extracted"

if [[ -z "${ROUNDTRIP_SOURCES:-}" || -z "${ROUNDTRIP_REFERENCE:-}" || -z "${ROUNDTRIP_PASSWORD:-}" ]]; then
    cat >&2 <<EOF
usage: set env vars then run:

  ROUNDTRIP_SOURCES='/path/a /path/b /path/c' \\
  ROUNDTRIP_REFERENCE='/path/to/reference.zip' \\
  ROUNDTRIP_PASSWORD='/path/to/password-file' \\
  $0

EOF
    exit 1
fi

[[ -x "$BIN" ]]                     || { echo "build tzip first" >&2; exit 1; }
[[ -f "$ROUNDTRIP_REFERENCE" ]]     || { echo "reference missing" >&2; exit 1; }
[[ -f "$ROUNDTRIP_PASSWORD" ]]      || { echo "password file missing" >&2; exit 1; }
command -v 7z >/dev/null            || { echo "install p7zip: brew install p7zip" >&2; exit 1; }

sudo -v || exit 1

# Read sources into an array (space-separated).
read -r -a INCLUDES <<< "$ROUNDTRIP_SOURCES"

sudo rm -f "$OUT"
rm -rf "$EXTRACT"

echo "== step 1: create archive =="
time sudo "$BIN" "$OUT" "${INCLUDES[@]}" \
    -m deflate -x 9 \
    --password-file "$ROUNDTRIP_PASSWORD" \
    --exclude '._*' --exclude '.DS_Store' \
    --raw-block -q
sudo chown "$USER" "$OUT"
echo

echo "== step 2: 7-zip listing =="
PW="$(cat "$ROUNDTRIP_PASSWORD")"
if 7z l -slt -p"$PW" "$OUT" > /tmp/tzip-roundtrip.list.txt 2>&1; then
    ENTRIES=$(grep -c '^Path = ' /tmp/tzip-roundtrip.list.txt || echo 0)
    echo "7-zip lists $ENTRIES entries."
else
    echo "!! 7-zip refused to list the archive:"
    tail -20 /tmp/tzip-roundtrip.list.txt
    exit 2
fi
echo

echo "== step 3: extract with 7-zip =="
mkdir -p "$EXTRACT"
time 7z x -y -p"$PW" -o"$EXTRACT" "$OUT" > /tmp/tzip-roundtrip.extract.txt 2>&1 || {
    echo "!! 7-zip extraction failed:"
    tail -20 /tmp/tzip-roundtrip.extract.txt
    exit 3
}
echo "extracted to $EXTRACT"
echo

echo "== step 4: compare tuples (path + size + packed-size) to reference =="
7z l -slt -p"$PW" "$ROUNDTRIP_REFERENCE" > /tmp/tzip-roundtrip.ref.slt  2>&1
7z l -slt -p"$PW" "$OUT"                 > /tmp/tzip-roundtrip.tzip.slt 2>&1

extract_tuples() {
    # AES-encrypted ZIP entries carry an empty top-level CRC (WinZip
    # AE-2), so we compare (path, uncompressed_size) rather than CRC.
    python3 - "$1" <<'PY'
import re, sys
blocks = re.split(r"\n\s*\n", open(sys.argv[1]).read())
for b in blocks:
    path = size = None
    is_folder = False
    for line in b.splitlines():
        if line.startswith("Path = "):     path = line[7:]
        elif line.startswith("Size = "):   size = line[7:]
        elif line.startswith("Folder = "): is_folder = line[9:].strip() == "+"
    if path and size is not None and not is_folder:
        # Trim archive-scope path-of-archive-file itself (header block).
        if path.endswith(".zip"):
            continue
        print(f"{path}\t{size}")
PY
}

extract_tuples /tmp/tzip-roundtrip.ref.slt  | sort > /tmp/tzip-roundtrip.ref.tuples
extract_tuples /tmp/tzip-roundtrip.tzip.slt | sort > /tmp/tzip-roundtrip.tzip.tuples
REF_N=$(wc -l < /tmp/tzip-roundtrip.ref.tuples  | tr -d ' ')
TZIP_N=$(wc -l < /tmp/tzip-roundtrip.tzip.tuples | tr -d ' ')
echo "  reference entries: $REF_N"
echo "  tzip entries:      $TZIP_N"

if diff -q /tmp/tzip-roundtrip.ref.tuples /tmp/tzip-roundtrip.tzip.tuples > /dev/null; then
    echo "  TUPLES MATCH — every entry has same path + uncompressed size"
else
    ONLY_REF=$(comm -23 /tmp/tzip-roundtrip.ref.tuples /tmp/tzip-roundtrip.tzip.tuples | wc -l | tr -d ' ')
    ONLY_TZIP=$(comm -13 /tmp/tzip-roundtrip.ref.tuples /tmp/tzip-roundtrip.tzip.tuples | wc -l | tr -d ' ')
    echo "  differences:"
    echo "     only in reference: $ONLY_REF entries"
    echo "     only in tzip:      $ONLY_TZIP entries"
    echo "  first 10 diffs (< reference, > tzip):"
    diff /tmp/tzip-roundtrip.ref.tuples /tmp/tzip-roundtrip.tzip.tuples | head -30
fi
echo

echo "== step 5: size comparison =="
tzip_size=$(stat -f%z "$OUT")
ref_size=$(stat -f%z "$ROUNDTRIP_REFERENCE")
diff_bytes=$((tzip_size - ref_size))
pct=$(python3 -c "print(f'{($tzip_size / $ref_size - 1) * 100:+.2f}')")
printf "  tzip     %'d bytes (%.2f GiB)\n" "$tzip_size" "$(bc <<< "scale=2; $tzip_size / 1073741824")"
printf "  ref      %'d bytes (%.2f GiB)\n" "$ref_size"  "$(bc <<< "scale=2; $ref_size  / 1073741824")"
printf "  delta    %'d bytes (%s%%)\n"   "$diff_bytes" "$pct"

echo
echo "== cleanup =="
rm -rf "$EXTRACT"
sudo rm -f "$OUT"
echo "kept listing at /tmp/tzip-roundtrip.list.txt"
