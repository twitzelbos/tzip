# tzip

A parallel ZIP/7z packer written in Rust. Designed for two things at once:

1. **Unspeakably insane throughput on modern CPUs** — libdeflater, hardware
   AES-256 and SHA-1, per-file rayon parallelism, parallel-block DEFLATE
   for single large files, zero-copy mmap, buffer pools, single-alloc AES
   envelope.
2. **Friendly to external drives** — a small, tunable reader pool (default 2
   threads) so USB and network-mounted filesystems don't get seek-stormed;
   optional macOS-specific fast paths (`getattrlistbulk`, `openat`,
   `F_RDADVISE`, `dispatch_io_read`) that reduce syscall count and metadata
   pressure on APFS.

Targets `p7zip a -tzip -mx=9 -mem=AES256` as its wire-format baseline —
archives are readable by 7-Zip, Keka, WinZip, PeaZip, iZip, and (for the
non-encrypted subset) `unzip`/macOS Archive Utility/Windows Explorer.

- Written in Rust (edition 2021)
- Single static binary, `cargo build --release`
- BSD-3-Clause

---

## Table of contents

- [Installation](#installation)
- [Quick start](#quick-start)
- [CLI reference](#cli-reference)
- [Compression methods](#compression-methods)
- [AES-256 encryption](#aes-256-encryption)
- [`.7z` solid archives](#7z-solid-archives)
- [Update mode](#update-mode)
- [Reading archives](#reading-archives)
- [Reading remote archives (HTTP / S3)](#reading-remote-archives-http--s3)
- [Searching across many archives (`tzip grep`)](#searching-across-many-archives-tzip-grep)
- [Finding entries across archives (`tzip find`)](#finding-entries-across-archives-tzip-find)
- [Mounting archives as a filesystem (`tzip mount`)](#mounting-archives-as-a-filesystem-tzip-mount)
- [TUI mode](#tui-mode)
- [Benchmarks (`tzip bench`)](#benchmarks-tzip-bench)
- [Performance](#performance)
- [External-drive tuning](#external-drive-tuning)
- [macOS-specific fast paths](#macos-specific-fast-paths)
- [Linux-specific fast paths](#linux-specific-fast-paths)
- [APFS-on-USB](#apfs-on-usb)
- [Known limitations](#known-limitations)
- [Architecture](#architecture)
- [Building from source](#building-from-source)
- [Testing](#testing)
- [License](#license)

---

## Installation

### From source

Requires a recent stable Rust toolchain (1.75+).

```
git clone git@github.com:twitzelbos/tzip.git
cd tzip
cargo build --release
# Binary at ./target/release/tzip
```

The release profile uses `-C target-cpu=native` (see `.cargo/config.toml`),
so hardware AES/SHA/CRC32 dispatch is baked into the build. Rebuild when
moving between machine generations.

Optional Cargo features:

```
# Linux: io_uring deep-queue reader (--io-uring), kernel >= 5.6
cargo build --release --features io-uring

# macOS: raw APFS block reader (--raw-block)
cargo build --release --features raw-apfs
```

The Linux FIEMAP extent-order reader (`--raw-block`) needs no feature flag —
it's always compiled in on Linux.

### System requirements at runtime

- macOS 11+ (universal support for ARMv8 crypto + libdispatch + `getattrlistbulk`)
- Linux with kernel 4.14+ (SHA-NI/AES-NI at runtime via `cpufeatures`). The
  FIEMAP extent-order reader (`--raw-block`) works on ext4/xfs/btrfs/f2fs; the
  optional `--io-uring` reader needs kernel ≥ 5.6
- Windows 10+ (build with `cargo build --release` from a POSIX-emulated shell
  such as MSYS2 — the bulk_walker and dispatch_io modules are compiled out
  automatically)

---

## Quick start

tzip 0.2 uses subcommands (`create`, `u`, `ls`, `cat`, `x`). This is a
breaking change from 0.1; the old flat `tzip <archive> <paths>...` form
is gone.

```
# Create a plain .zip archive
tzip create out.zip path/to/source_tree

# With max DEFLATE and AES-256
tzip create out.zip src/ -m deflate -x 12 -p 'hunter2'

# Zstd for high ratio + speed
tzip create out.zip src/ -m zstd -x 19

# .7z solid LZMA2 with header encryption
tzip create out.7z src/ --solid -p 'hunter2'

# Deterministic byte-identical builds
tzip create out.zip src/ --sort

# Add a file to an existing archive without re-compressing anything else
tzip u out.zip extra_file.txt

# Remove entries by glob (triggers a byte-copy rebuild, atomic rename)
tzip u --delete '*.log' out.zip

# Reading side: list, stream one file, extract
tzip ls out.zip
tzip cat out.zip src/main.rs > main.rs
tzip x out.zip --to ./extracted
tzip cat out.zip secret.txt -p 'hunter2'
```

---

## CLI reference

tzip 0.2 uses subcommands:

```
tzip create <ARCHIVE> <PATHS>...   # create a new archive
tzip u      <ARCHIVE> [PATHS]...   # update: add / replace / delete entries
tzip ls     <ARCHIVE>              # list entries
tzip cat    <ARCHIVE> <NAME>       # stream one entry to stdout
tzip x      <ARCHIVE> [NAMES]...   # extract (whole archive or selected names)
```

Aliases: `a` for `create`, `l` for `ls`, `extract` for `x`.

The flags below are for `create` unless the row says otherwise. `u`
takes the same subset relevant to its work (jobs, read-jobs, walk-jobs,
method/level for new entries, password, sort, exclude, quiet, verbose,
keep-cache, basename-only) plus `--delete GLOB` and `--atomic`.
The reader subcommands (`ls`, `cat`, `x`) take only password-related
flags (`-p`, `--password-file`), `-q`, `-v`, plus `cat`'s `<NAME>`
positional and `x`'s optional `[NAMES]...` + `--to DIR`.

**Positional (create / u)**

| arg | meaning |
|---|---|
| `<ARCHIVE>` | Archive path. `.7z` extension auto-enables `--solid` for `create`. |
| `<PATHS>...` | Files or directories to archive. For `u`, may be empty when `--delete` is given. |

**Compression & encryption**

| flag | default | meaning |
|---|---|---|
| `-m, --method <NAME>` | `deflate` | `store`, `deflate`, `bzip2`, `lzma`, `xz`, `zstd` |
| `-x, --level <N>` | `6` | Level; range per method (auto-clamped): deflate 0-12, bzip2 1-9, lzma/xz 0-9, zstd 1-22 |
| `-p, --password <PW>` | none | AES-256 password. Use `-` to read from stdin (tty prompt if interactive) |
| `--password-file <FILE>` | | Read password from the first line of `<FILE>` |
| `--solid` | off | Emit `.7z` (LZMA2 solid, optional header encryption). Auto-set when archive ends in `.7z` |

**Concurrency**

| flag | default | meaning |
|---|---|---|
| `-j, --jobs <N>` | physical cores | CPU workers (compress + encrypt) |
| `--read-jobs <N \| auto>` | `2` | Reader threads. `auto` = `min(cpu_jobs, 8)` — safe on internal NVMe, avoid on external USB drives |
| `--walk-jobs <N>` | `min(cpu_jobs, 4)` | Directory-scan threads (jwalk backend only) |

**macOS fast paths**

| flag | default | meaning |
|---|---|---|
| `--classic-walk` | off | Use portable jwalk walker instead of the `getattrlistbulk` bulk walker (only useful for debugging) |
| `--dispatch-io` | off | Route small-file reads (<1 MiB) through GCD `dispatch_io_read` instead of blocking `read()` |
| `--keep-cache` | off | Skip `F_NOCACHE` — keep read blocks in the OS page cache. Useful on APFS-USB where page-cache bypass hurts more than helps |
| `--raw-block[=true\|false]` | auto (root+APFS-on-USB) | Read files by parsing APFS on-disk format directly from `/dev/rdiskN` (bypasses VFS + AV hooks). Requires root. Auto-enabled when eligible. See [`docs/RAW_BLOCK.md`](docs/RAW_BLOCK.md). Requires the `raw-apfs` Cargo feature. |

**Linux fast paths**

| flag | default | meaning |
|---|---|---|
| `--raw-block[=true\|false]` | auto (rotational/removable media) | Extent-order reader: use the `FS_IOC_FIEMAP` ioctl to sort files by physical block offset and read them in disk order, turning a seek storm into a forward sweep. Works on ext4/xfs/btrfs/f2fs. Big win on spinning / USB media, neutral on NVMe (so auto-enabled only on seeking media). See [`docs/LINUX.md`](docs/LINUX.md). |
| `--io-uring` | off | Read file bodies via io_uring at queue depth 128 instead of one blocking `read` per reader thread — keeps NVMe busy from a single thread. Pairs with `--raw-block` (files submitted in disk order). Requires the `io-uring` Cargo feature and kernel ≥ 5.6. |
| `--apfs-device <DEV>` | off | Archive an **APFS** volume by parsing it directly off a raw device (e.g. `/dev/sda1`) — no mount, no APFS driver needed. Positional args become in-volume paths; omit them for the whole volume. Requires the `raw-apfs` Cargo feature + read access to the device. See [`docs/LINUX.md`](docs/LINUX.md). |

**Output selection & ordering**

| flag | default | meaning |
|---|---|---|
| `--sort` | off | Deterministic order — same inputs produce byte-identical archives (STORE mode; AES randomizes salt so byte-identical AES output requires additional work) |
| `--exclude <GLOB>` | none | Repeatable simple glob (`*` `?`) matched against the archive path |

**Update subcommand (ZIP only; `tzip u`)**

| flag | default | meaning |
|---|---|---|
| `--delete <GLOB>` | | Repeatable. Delete archive entries matching this glob. |
| `--atomic` | off | Force rebuild-to-temp + atomic rename. Without it, pure-add updates are applied in place via truncate-append + an on-disk undo file. |

See [Update mode](#update-mode) for the two-mode design (append vs rebuild) and the crash-recovery story.

**Progress**

| flag | default | meaning |
|---|---|---|
| `-q, --quiet` | off | No progress bar |
| `-v, --verbose` | off | Print each added file to stderr |
| `--tui` | off | Ratatui dashboard (per-worker MB/s, overall gauge, `q`/Esc to cancel) when stdout is a TTY |

Full help: `tzip --help`.

---

## Compression methods

All write-side methods compose orthogonally with AES-256.

| method | ZIP ID | r/w | crate | notes |
|---|---|:-:|---|---|
| `store` | 0 | r+w | — | Uncompressed. Fast; use for pre-compressed data (JPEGs, MP4, `.gz`) |
| `shrink` | 1 | r | `compcol` | PKZIP 1.x LZW variant (9-13 bit, partial clear). Vintage archives only |
| `reduce` | 2-5 | r | `compcol` | PKZIP 1.x LZ + Huffman, factors 1-4. Vintage archives only |
| `implode` | 6 | r | `compcol` | PKZIP 1.x Shannon-Fano + 4/8 KiB window. The dominant pre-Deflate method |
| `deflate` | 8 | r+w | `libdeflater` | Default. 30-40% faster than flate2. Levels 0-12; 12 ≈ zopfli-lite |
| `deflate64` | 9 | r | `compcol` | PKWARE's 64 KiB-window variant. Common in Windows-produced ZIPs (WinZip, `.NET` ZipArchive on large entries) |
| `bzip2` | 12 | r+w | `bzip2` | 5-10% better than DEFLATE on text; ~4× slower |
| `lzma` | 14 | r+w | `xz2` (LZMA1) | Better ratio than DEFLATE; ZIP consumers need LZMA support |
| `xz` | 95 | r+w | `xz2` (LZMA2) | Same LZMA2 as `.7z`, but per-file inside a ZIP |
| `zstd` | 93 | r+w | `zstd` | Great ratio + speed. Recipient needs zstd-aware unzipper |
| `ppmd` | 98 | r | `compcol` | PPMd variant Ir1 (context-modelling entropy coder). Rare in the wild but valid per APPNOTE |

Shrink / Reduce / Implode are read-only — modern tooling (Info-ZIP,
libarchive, miniz, `zip` crate) all dropped encoder support decades
ago and there is no reason to produce new archives in these formats.
tzip still opens them so a 1990-era PKZIP 1.x archive is readable.
See [Reading archives](#reading-archives) for the subcommands.

### Compatibility matrix (method + AES-256 → who can read it)

| combination | 7-Zip | Keka | WinZip | macOS AU | Windows Explorer | `unzip` |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| STORE / DEFLATE + AES | yes | yes | yes | no | no | no (patched builds only) |
| BZIP2 + AES | yes | yes | yes | no | no | no |
| LZMA / XZ + AES | yes | yes | partial | no | no | no |
| Zstd + AES | fork | yes | partial | no | no | no |
| `.7z` + AES | yes | yes | yes | no | no | no |

macOS Archive Utility and stock Windows Explorer never handle AES ZIP —
recipients always need a real unzipper.

---

## AES-256 encryption

tzip implements WinZIP AES-256 (spec 6.3.0 §7.2, AE-2 variant):

- Compress-then-encrypt
- Salt: 16 random bytes per entry
- KDF: PBKDF2-HMAC-SHA1, 1000 iterations, derives 32 (enc) + 32 (mac) + 2 (verify) bytes
- Cipher: AES-256-CTR with a 128-bit little-endian counter starting at 1
- MAC: HMAC-SHA1 over the ciphertext, truncated to 10 bytes
- Extra field: `0x9901` with vendor ID `AE`, strength byte `0x03` (AES-256), version `0x0002` (AE-2)
- AE-2 stores CRC-32 = 0 in the wire header per spec; tzip still computes it internally

The archive body layout for each encrypted entry:

```
[ salt(16) | pw_verify(2) | compressed+encrypted body | HMAC-SHA1(body)[..10] ]
```

Password sourcing (in precedence order):
- `--password-file` (first line of file)
- `--password -` (interactive tty prompt)
- `--password <pw>` (arg, visible in process list; prefer `--password-file` in scripts)

### Multi-disk (spanned) archives

tzip reads archives split across `base.z01`, `base.z02`, ...,
`base.zip` — the format Info-ZIP's `zip -s`, and ancient PKZIP for
floppy spanning, produce. Point tzip at the last disk (`.zip`) and
it opens the siblings automatically:

```
tzip ls      bigarchive.zip                    # lists across all disks
tzip cat     bigarchive.zip path/entry.log     # even if the body spans disks
tzip x       bigarchive.zip --to ./extracted   # parallel workers open their own stitched views
```

Local files only — the sibling disks must live in the same
directory as the `.zip`. HTTP and S3 multi-disk aren't supported;
no standard cloud layout addresses them anyway.

### Reading ZipCrypto-encrypted archives (vintage password protection)

tzip also **reads** entries encrypted with traditional PKZIP
encryption ("ZipCrypto") — the format `zip -e`, PKZIP 2.x, and most
"password-protected ZIP" dialogs produce. Pass the password the same
way as for AES. ZipCrypto is cryptographically broken (a known-
plaintext attack recovers the key in seconds); tzip supports reading
it only to open vintage archives. New encryption always goes through
AES-256.

Detection is automatic — tzip reads GP bit 0 + method ID to tell
ZipCrypto apart from AES. Wrong-password detection relies on the
format's single-byte header check (1/256 false-pass rate per the
spec); downstream decompression usually fails noisily if a wrong
password happens to pass the check.

---

## `.7z` solid archives

`--solid` (or archive extension `.7z`) switches to `sevenz-rust` and emits an
LZMA2 solid stream:

- Substantially better ratio on similar-file corpora (logs, source
  trees, JSON dumps) — the dictionary spans all files
- With a password, header encryption is enabled (filenames are hidden)
- **Single-threaded**: sevenz-rust 0.6 does not expose multi-threaded LZMA2
  (`-mmt=on` in p7zip). `--jobs` is ignored for `.7z` output.

Workarounds if speed matters more than ratio on `.7z`:
- Use `-m xz` inside a `.zip` (still per-file parallel across files)
- Shell out to system `p7zip` with `7z a -mmt=on -m0=lzma2`

---

## Update mode

`-u` / `--update` modifies an existing `.zip` in place without re-reading or
re-compressing the entries that are being kept. It's the analog of
`7z u archive.zip files...`. ZIP only — `.7z` solid archives can't be
edited in place and tzip rejects `-u` on them up front.

```
# Add c.txt to an existing archive
tzip -u out.zip c.txt

# Replace an entry by providing a matching input
tzip -u out.zip a.txt             # if a.txt is already in the archive, it is replaced

# Remove entries by glob
tzip -u --delete '*.log' out.zip

# Mixed: delete some, add some
tzip -u --delete b.txt out.zip d.txt

# Force the safer path (see below) even for pure adds
tzip -u --atomic out.zip c.txt
```

### Two paths, chosen automatically

tzip picks between two paths for every update, based on what the operation
actually needs to do. `-v` prints the chosen mode (`append` / `rebuild`)
and a `(add N, replace N, delete N, keep N)` breakdown.

| path | when | what happens |
|---|---|---|
| **append** | pure add with no name collisions, no `--delete`, no `--sort`, no `--atomic` | The old central directory is truncated off the end of the archive. New LFHs + bodies are written starting at the old CDR's offset. A fresh CDR covering (old entries + new entries) is written at the end. **Zero bytes of existing data are rewritten.** |
| **rebuild** | `--delete`, replace, `--sort`, or `--atomic` | Write the full new archive to `<archive>.tzip-tmp-<pid>`; kept entries are byte-copied LFH + body (no decrypt, no decompress); new entries flow through the normal parallel pipeline; `sync_all`; atomic `rename()` over the original. |

Both paths skip re-compression and re-encryption of kept entries. The
difference is append-mode avoids rewriting the kept bytes at all.

### Crash safety

Append mode writes `<archive>.tzip-undo` before touching the archive. The
undo file holds the original central-directory + EOCD bytes and a header
recording the archive's original size. On success the undo file is
deleted; on crash, the next `tzip -u` invocation detects the undo file,
compares the archive's size to the recorded "original" size, and either:

- silently removes the undo (archive is intact; a successful prior run
  failed to clean up), or
- truncates the archive back to the pre-append offset, writes the saved
  CDR + EOCD bytes, syncs, and prints `tzip: recovered ... from
  interrupted update` before proceeding.

Rebuild mode is inherently atomic — the temp file either lands via
`rename()` or is removed on error; the original archive is never modified.

### What is preserved

- WinZIP AES-256 envelopes on kept entries: salt, PBKDF2-derived verifier,
  ciphertext, and HMAC tag are copied byte-for-byte. No password is
  required to update an AES archive when only adding or carrying entries.
- ZIP64 entries (including `lfh_offset ≥ 4 GiB` and files `≥ 4 GiB`).
- UTF-8 filenames (GP bit 11) and the raw name bytes.
- LFH extra fields — extended timestamps (`0x5455`), Unix uid/gid
  (`0x7875`), and any other extras — because the entire LFH is copied
  verbatim. (Unknown central-directory extras are not preserved; the CDR
  is reconstructed from parsed fields.)

### Known limits

- `.7z` is rejected; rebuild it from source if you need to change its
  contents.
- Concurrent writers are not supported — two `tzip -u` runs on the same
  archive will race. The undo file's exclusive-create catches the simple
  overlap case (the second run fails with a clear "undo already exists"
  error).
- Case-sensitive name collision detection. Two entries differing only in
  case will both exist.

## Reading archives

Three read subcommands land in 0.2, with the long-range plan of
extending them to remote sources (HTTP Range, S3) and to a mount point
in later releases. Today they work on local `.zip` files.

```
# List entries. `*` marks AES-encrypted entries.
tzip ls out.zip

# Stream one entry to stdout. Works with any method tzip can write
# (store / deflate / bzip2 / lzma / xz / zstd), plus AES-256 AE-2.
tzip cat out.zip src/main.rs > main.rs
tzip cat out.zip secret.txt -p 'hunter2'

# Extract. Default destination is the current directory.
tzip x out.zip --to ./extracted
tzip x out.zip a.txt nested/c.txt --to ./extracted     # selected entries only
tzip x aes.zip --to ./extracted -p 'hunter2'

# Parallel extraction (default -P 8). Each worker holds its own
# backing source, so HTTP/S3 Range GETs and decompression overlap.
tzip x out.zip --to ./extracted -P 16
tzip x s3://bucket/huge.zip --to ./extracted -P 32 --connections 16
```

Extraction rejects *zip-slip* entries (any archive entry whose name
contains `..`, is absolute, or looks like a Windows drive/UNC path)
before touching the filesystem. Partial writes on error are deleted.
Typo'd entry names fail fast before anything is written.

`-P N` controls concurrent entries in flight (default 8); each worker
opens its own backing source (its own fd for local archives, its own
HTTP/S3 client for remote). On HTTP/S3 this is the big win — many
Range GETs overlap instead of serialising through one connection.
`--connections N` caps worker count below `-P` if you need a smaller
HTTP pool; it is a no-op on local archives.

Wrong password on a `cat` fails *before* any plaintext hits stdout —
the AES-256 verifier mismatch is checked before the CTR stream starts.
Corrupted/tampered ciphertext passes through the stream (CTR is
malleable at the byte level) and fails at the end with
`AES-256 HMAC mismatch — archive is tampered or corrupted`; downstream
should treat whatever bytes made it to disk/stdout as untrusted.

## Reading remote archives (HTTP / S3)

`tzip ls`, `tzip cat`, and `tzip x` accept URLs in place of the local
path. The HTTP(S) and S3 code paths use `Range` requests to pull just the
bytes they need — one GET for the EOCD (~65 KiB at the tail), one for
the central directory, one more per entry you actually `cat` or extract.

```
# List the entries in a ZIP sitting at a URL.
tzip ls https://example.com/releases/archive.zip

# Stream one file out of it — only the bytes for that entry transit.
tzip cat https://example.com/releases/archive.zip LICENSE

# Same against an S3 object. Virtual-hosted-style URL under the hood.
tzip ls s3://my-bucket/path/to/archive.zip
tzip cat s3://my-bucket/path/to/archive.zip README.md

# AES-encrypted entries work exactly the same — decrypt happens locally.
tzip cat s3://my-bucket/vault.zip secrets/api-key.txt -p 'hunter2'
```

Why this matters: `tzip cat s3://bucket/huge.zip one-file.txt` pulls
~200 KiB over the wire regardless of whether `huge.zip` is 100 MB or
100 GB. No staging, no full download.

Write operations (`create`, `u`) are **local-only** by design — passing
a URL target fails fast with a clear error.

### HTTP(S) requirements

The server must support `Accept-Ranges: bytes` and return `206 Partial
Content` on Range requests. `python3 -m http.server` doesn't (it
ignores Range); any production static-file server (nginx, S3, GCS,
Cloudflare R2, GitHub raw, etc.) does.

### S3 credentials

tzip reads credentials in this order:

1. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` env vars (+ optional
   `AWS_SESSION_TOKEN` for STS / role-chain temporary creds).
2. `~/.aws/credentials` — INI file with `[default]` (or `[<profile>]`
   selected via `AWS_PROFILE`) containing `aws_access_key_id` /
   `aws_secret_access_key` / optional `aws_session_token`.

Region: `AWS_REGION` → `AWS_DEFAULT_REGION` → `~/.aws/config` for the
chosen profile → fallback `us-east-1`.

**MinIO / LocalStack / S3-compatible endpoints**: set `AWS_ENDPOINT_URL`
to override the default `https://<bucket>.s3.<region>.amazonaws.com`
host. Path-style URLs (`http://host:port/<bucket>/<key>`) are used
automatically when the endpoint override is in effect.

Not supported in this release: SSO, IAM instance metadata (IMDSv2),
AssumeRole chains. If you need these, drop temporary creds into
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN` via
whatever tool you already use (`aws sso login`, `aws-vault exec`, etc.)
and tzip will pick them up.

## Searching across many archives (`tzip grep`)

`tzip grep PATTERN <archive>...` fans a regex search out across many
ZIPs in parallel — local, HTTP(S), or S3. CDR-side filters skip
entries at the directory without ever fetching their bodies; the
Range-GET primitive means only matching entries' bytes transit.
Checkpoint/resume keeps multi-hour jobs recoverable.

```
# One archive, basic grep.
tzip grep ERROR out.zip

# Fan-out across many; parallelism knobs tuned for S3's soft limits.
tzip grep ERROR s3://my-bucket/logs/*.zip -P 64 --inner-jobs 8

# CDR filters: skip entries that can't match before fetching bodies.
tzip grep ERROR s3://bucket/*.zip \
    --include '*.log' --since 2026-11-01 --min-size 1024

# Checkpoint + resume: a multi-hour job survives a Ctrl-C / SIGKILL.
tzip grep ERROR s3://bucket/logs/*.zip --checkpoint run.ck
# ... Ctrl-C ...
tzip grep ERROR s3://bucket/logs/*.zip --checkpoint run.ck --resume

# Delegate matching-entry output to ripgrep for its full feature set
# (PCRE2, context lines, color, --json).
tzip grep ERROR s3://bucket/*.zip \
    --exec 'rg -C 3 --color=always {}'

# Structured output for pipelining.
tzip grep ERROR s3://bucket/*.zip --json | jq '.'
```

### What tzip owns vs. what it reuses

Orchestration is tzip's job: fan-out, CDR-side filtering (so you
never pay for bytes that can't match), remote sources via the
existing HTTP/S3 reader stack, and checkpoint/resume. The actual
regex engine is the **`regex` crate** — the exact engine ripgrep
ships, factored as a reusable library. You get rg's regex syntax and
speed without the "reinvented a regex engine" footgun.

For features the regex crate doesn't cover (PCRE2, `-C/--context`,
colored output, `--json` match context), `--exec CMD` writes each
matched entry to a tempfile and runs `CMD` with `{}` → path. Only
entries that passed tzip's regex are forwarded, so the external
command acts as an rg pre-filter.

### Parallelism knobs

| flag | default | meaning |
|---|---|---|
| `-P, --parallel <N>` | 32 | Concurrent archives in flight. |
| `--inner-jobs <N>` | 8 | Concurrent entries scanned within one archive. Each inner worker opens its own backing source so Range GETs overlap. |
| `--connections <N>` | `P × inner` capped at 256 | Shared HTTP/S3 connection pool cap. |

Defaults are tuned for S3's documented per-prefix soft limits
(~5,500 GET/s per prefix historically). Override per workload.

### CDR-side filters

All applied at the central directory before any body fetch — on S3,
every skipped entry is a saved Range GET.

| flag | meaning |
|---|---|
| `--include GLOB` | Only scan entries matching the glob. Repeatable. |
| `--exclude GLOB` | Skip entries matching the glob. Repeatable. |
| `--min-size BYTES` | Skip entries smaller than this. |
| `--max-size BYTES` | Skip entries larger than this. |
| `--since YYYY-MM-DD` | Skip entries with mtime before the given UTC date. |
| `--until YYYY-MM-DD` | Skip entries with mtime after the given UTC date. |

### Checkpoint and resume

`--checkpoint FILE` writes a JSONL file with:

- **Header line** — the query (pattern, filters, archive set).
- **One line per completed `(archive, entry)`** with match count + timestamp.

`--resume` re-opens the same file, validates the header matches the
current invocation (refuses with a clear error if pattern or filters
changed), and skips `(archive, entry)` pairs already present. Output
isn't duplicated on resume.

Granularity is per-entry, not per-byte. An entry interrupted mid-scan
restarts at byte 0 of that entry on resume — worst case ~one entry
per worker is re-scanned. For log-archive workloads where entries are
distinct files, this is essentially free; byte-offset resume would
need deterministic match positions across decompression or format-
specific restart markers, neither of which pay for themselves.

Tune `--checkpoint-every N` (default 1) up if your corpus is many
tiny entries and per-entry `fdatasync` dominates.

### Output

Default is grep-style, one line per match:

```
s3://bucket/logs/2026-11-22.zip:api/access.log:4521:[2026-11-22T14:05:11Z] ERROR connection refused
```

`--files-only` collapses to `archive:entry` once per entry with any
match. `--json` emits `{"archive", "entry", "line", "match"}` one per
match. Lines are written under a shared stdout lock so parallel
workers don't interleave mid-line.

### S3 bucket globbing

`s3://bucket/prefix/*.zip` and friends are expanded via
ListObjectsV2 (paginated) before fan-out. Everything before the first
`*` or `?` becomes the S3 prefix passed to the API, so you don't
list the whole bucket just to filter one subtree. Local file globs
are still handled by your shell.

### Known limits

- **Per-entry memory is bounded by the longest line.** The scanner is
  streaming: decompressed output is scanned chunk-by-chunk, only the
  trailing partial line stays in memory. A 10 GB log of normal-length
  lines costs well under a megabyte in the scanner. `--exec` tees
  decompress output straight to a tempfile as it comes out, so the
  external-command path is also bounded now. (AES-encrypted entries
  still materialise the plaintext at the AES layer before decompress —
  see the AES comment below.)
- **AES entries materialise plaintext**: AES-256 AE-2 decryption
  verifies HMAC over the whole ciphertext, so the plaintext must be
  held in memory at the AES layer. The scanner is still streaming on
  top, so you save the uncompressed body but not the compressed one.
- **`--exec` process spawn overhead** (~1-10 ms per spawn on Linux,
  ~5-20 ms on macOS). Fine for typical workloads; defer to the
  built-in matcher if you have many small matched entries per
  archive.
- **Output ordering is non-deterministic** with parallel workers.
  Use `-P 1 --inner-jobs 1` for stable order, or pipe through
  `--json | jq -s 'sort_by(.archive, .entry, .line)'`.

## Finding entries across archives (`tzip find`)

`tzip find <archive>...` lists entries that match CDR-side filters
(name glob, size range, mtime range) without ever fetching an entry
body. For remote archives that's a single HEAD + one CDR fetch per
archive, then pure in-memory filtering — orders of magnitude cheaper
than `grep` when you only need "which entries are in here."

```
# Everything in a local archive (tabular: archive, mtime, size, name).
tzip find out.zip

# Just entry names (good for piping into xargs).
tzip find out.zip --names-only

# Across 1000 S3 archives in parallel, filtered to .log files
# bigger than 1 MiB modified since 2026-01-01.
tzip find s3://bucket/logs/*.zip -P 64 \
  --include '*.log' --min-size 1048576 --since 2026-01-01

# JSON for jq / downstream tooling.
tzip find s3://bucket/*.zip --json | jq -s 'map(.size) | add'
```

Flags:

| Flag | What it does |
| --- | --- |
| `-P N` | Concurrent archives in flight. Default 32. |
| `--include GLOB` | Only entries matching GLOB (repeatable). |
| `--exclude GLOB` | Skip entries matching GLOB (repeatable). |
| `--min-size N` / `--max-size N` | Uncompressed-size range, bytes. |
| `--since DATE` / `--until DATE` | mtime range, `YYYY-MM-DD` (UTC). |
| `--names-only` | One `archive:entry` per line, nothing else. |
| `--json` | JSONL with `archive`, `entry`, `size`, `compressed_size`, `method`, `encrypted`, `mtime`, `crc32`. |

S3 bucket-globbing is the same as `tzip grep`: prefix expansion via
ListObjectsV2. Local shell globs are handled by your shell. `find` has
no body-fetching inner loop, so it skips `grep`'s `--inner-jobs` /
`--connections` / `--checkpoint` / `--exec` flags.

## Mounting archives as a filesystem (`tzip mount`)

`tzip mount <archive> <mountpoint>` serves any ZIP as a read-only
filesystem via FUSE3. Every shell tool — `cat`, `grep`, `ripgrep`,
`vim`, `find`, `git` — works against the mount transparently. Combined
with the S3 reader, you can mount a 100 GB archive from S3 locally and
read individual files out of it without staging anything:

```
mkdir /tmp/mnt
tzip mount s3://my-bucket/huge.zip /tmp/mnt &
cat /tmp/mnt/path/to/entry.log           # Range GETs only the needed bytes
rg PATTERN /tmp/mnt/                     # same, bodies decrypted on access
fusermount3 -u /tmp/mnt                  # unmount (or Ctrl-C the tzip mount)
```

Also works on local files and HTTP URLs:

```
tzip mount out.zip /tmp/mnt &
tzip mount https://example.com/release.zip /tmp/mnt &
```

### Flags

| flag | default | meaning |
|---|---|---|
| `--cache-mb <N>` | 256 | Memory budget for the per-entry decompressed-body LRU cache. Soft limit — one oversized entry won't force a thrash, just briefly exceeds the budget. |
| `--allow-other` | off | Add the `allow_other` FUSE option so other users on the host can read the mount. Needs `user_allow_other` in `/etc/fuse.conf`. |
| `--write` | off | Open the mount read-write. Works on Linux FUSE3 and macOS FSKit (15.4+). See *Writable mounts* below. |
| `-p, --password <PW>` | | AES password for encrypted entries. `--password-file` reads from file. |

### Semantics

- **Default is read-only.** Writes fail with `EROFS` unless you
  pass `--write`.
- **First access decompresses.** The first read of an entry decompresses
  the whole body into RAM; subsequent reads (same entry, any offset) are
  RAM-speed memcpy. The LRU evicts cold entries to stay under `--cache-mb`.
- **AES decryption happens on access** — the password stays in-process
  for the life of the mount. Only entries you actually read get
  decrypted.
- **Unmount** via Ctrl-C on the foreground `tzip mount` or
  `fusermount3 -u <mountpoint>` from another shell.

### Writable mounts (`--write`)

Works on **Linux FUSE3** and **macOS FSKit (15.4+)** with the same
semantics. Opt in with `--write`:

```
tzip mount out.zip /tmp/mnt --write &
echo "edit through the mount" > /tmp/mnt/notes/new.md
rm /tmp/mnt/stale-logs/*.log
mkdir /tmp/mnt/fresh-dir
# Unmount via Ctrl-C on the `tzip mount`, or:
fusermount3 -u /tmp/mnt    # Linux
umount /tmp/mnt            # macOS
```

Semantics (same on both platforms):

- **All edits buffer in memory.** `create`, `write`, `unlink`,
  `mkdir`, `rmdir`, and `truncate` modify an in-RAM journal — the
  on-disk archive is untouched.
- **Commit happens on unmount.** When the session ends, the journal
  is replayed through `tzip u --atomic`: a new archive builds in a
  tempfile, then renames over the original. Partial commits can't
  happen.
- **A crash before unmount loses the pending changes** but never
  corrupts the archive — the original is only overwritten by a
  fully-formed replacement.
- **Supported ops**: create, write, unlink, mkdir, rmdir, truncate,
  **rename** (files, dirs including whole-subtree moves, cross-
  directory), **chmod** (persisted via `external_attr`), and
  **setting mtime** (persisted via DOS date/time). `touch`,
  `cp -p`, `install -m`, and `mv` all do the right thing and
  round-trip through commit.
- **Still not supported**: chown (UID/GID — ZIP has extra fields
  for this but tzip ignores them for now), symbolic links, hard
  links. `chown` is accepted silently so `cp -p` doesn't error,
  but the ownership change doesn't persist.
- **Local archives only.** `--write` on an `http://`, `https://`,
  or `s3://` archive is ignored and the mount stays read-only
  (noted in the mount-startup banner — FSKit also fails loudly at
  `loadResource` if someone bypasses the CLI check).

### Platform support

- **Linux (FUSE3)** — fully working. Install `fuse3` via your package
  manager (`apt install fuse3`, `dnf install fuse3`, etc.).
- **macOS (FSKit, macOS 15.4+)** — fully working after a one-time
  setup. The host build needs Xcode CLI tools (`swiftc`, `codesign`)
  and `--features fskit`. One-time setup:

  ```
  cargo build --release --features fskit
  scripts/build-fskit-bundle.sh                # builds + signs tzip.fs
  ./target/release/tzip mount --install         # approve in System Settings
  ```

  Then day-to-day:

  ```
  tzip mount s3://my-bucket/archive.zip /tmp/mnt &
  rg PATTERN /tmp/mnt/
  umount /tmp/mnt                                # or Ctrl-C the tzip mount
  ```

  **One OS policy gate**. On stock SIP-enabled macOS,
  `OSSystemExtensionManager` only accepts activation requests whose
  host binary is signed with a Developer ID (and whose extension
  bundle is notarized). For local dev without a $99/year Developer
  Program account, enable developer mode:

  ```
  csrutil disable                # from Recovery mode (one-time)
  sudo systemextensionsctl developer on
  ```

  Then ad-hoc-signed builds install. A properly signed + notarized
  distribution build that works on stock macOS is a packaging
  follow-up — not a code change.

### Known limits

- **Inode numbers are stable within one mount session but not across
  remounts.** Shell tools that rely on persistent inodes (very rare)
  will get confused.
- **Concurrent reads of different entries serialise** through a single
  archive-source mutex. A follow-up can pool sources per-mount for
  parallel Range GETs (mirroring `tzip grep --inner-jobs`).
- **mtime from the DOS-epoch default** if the archive was created by
  tzip's macOS bulk walker — same limitation documented elsewhere.
- **Symlinks** aren't surfaced as symlinks (ZIP writer doesn't emit
  them yet). Follow-up.

## TUI mode

`--tui` when stdout is a TTY enables a ratatui alternate-screen
dashboard: overall progress, a live throughput sparkline with peak
MB/s and active-worker count, and one line per compressor worker
showing the file currently being encoded and its instantaneous rate.

```
┌ tzip ─────────────────────────────────────────────────────────────────┐
│ 2.3 GB / 9.1 GB  •  38k / 143k files  •  412 MB/s  •  ETA 0m17s       │
│ [===============================>............................] 25%   │
├ throughput  •  peak 620 MB/s  •  workers 8/10 ────────────────────────┤
│ ▁▂▄▇█▇▆▆▆▆▇▇█▇▆▆▆▅▄▄▃▃▂▂▂▃▄▅▆▆▇▇██▇▇▆▆▆▆▆▅▅▄▄▃▃▂▂▁▁▁▁▁▁▁▁▁▁▁         │
└───────────────────────────────────────────────────────────────────────┘
┌ workers ──────────────────────────────────────────────────────────────┐
│ worker  0: ▶ .../subdir-a/file-001.dat                       511 MB/s │
│ worker  1: ▶ .../subdir-b/file-023.dat                       489 MB/s │
│ worker  2: · idle                                              0 MB/s │
│ ...                                                                   │
└───────────────────────────────────────────────────────────────────────┘
```

In streaming mode the walker doesn't know the total upfront, so the
top row switches to `1.4 GB written • 10k files • 341 MB/s •
discovering…` and the gauge stays at 0% until the walker finishes —
the sparkline and worker rows still show live activity.

Keys: `q` or Esc to signal cancel. Cancel is currently cooperative
— the pipeline finishes in-flight items before exiting.

---

## Benchmarks (`tzip bench`)

`tzip bench` measures tzip against system `7z`, `zip`, and `unzip`
on a reproducible corpus. Three benchmarks: **create**, **extract**,
**list**. External tools that aren't on PATH are skipped (noted in
the header line).

```
# Default: ~50 MB synthetic corpus, 3 iterations, human-readable table.
tzip bench

# ~5 MB corpus, 1 iteration — CI-time sanity check.
tzip bench --quick

# Against your own directory (any contents).
tzip bench --corpus ./my-code -n 5

# JSON lines for scripting / charting.
tzip bench --json | jq -r 'select(.benchmark=="create") | [.tool, .throughput_mib_per_s] | @tsv'

# Just one section.
tzip bench --only extract
```

Output includes **median**, **min**, **max** wall time, **throughput
(MiB/s)**, and for the create section also **output size** and
**compression ratio**. Each tool runs against its own
self-created archive so the extract column reflects the real
end-to-end pipeline, not just the decompressor. The synthetic
corpus is deterministic (seeded PRNG, fixed layout) — rerunning on
the same host gives repeatable numbers.

**Caveat on tiny corpora**: `--quick`'s ~5 MB input is below the
regime where tzip's fan-out architecture pays for its thread-pool
startup. Use the default (~50 MB) or `--corpus <big dir>` to see
realistic numbers. CI uses `--quick` as a correctness smoke test,
not a perf claim.

---

## Performance

### Design summary

- **Output preallocation.** At archive open, tzip reserves extents on
  disk via `F_PREALLOCATE` (macOS) / `fallocate(FALLOC_FL_KEEP_SIZE)`
  (Linux) sized to `1.05 × uncompressed_bytes` in batch mode, or 256 MiB
  in streaming mode. Unused tail is trimmed via `ftruncate` at finish.
  Big win on exFAT (single-extent output vs fragmented growth).
- **FS-aware writer buffer.** `BufWriter` capacity defaults to 4 MiB but
  scales up to 16 MiB on exFAT/msdos to align with large clusters, and
  scales with `f_iosize` on other filesystems.
- **Per-file parallelism.** Rayon-sized pool of CPU workers (default =
  physical core count). Each worker owns a `Scratch` with a reusable
  DEFLATE output buffer and cached `libdeflater::Compressor` (no
  per-file allocation).
- **Split reader/CPU pool.** Reader threads (default 2) own the disk;
  CPU threads (default = physical cores) own compression + AES. Bounded
  crossbeam channels backpressure the readers if the CPU or writer
  stalls.
- **Streaming walk.** The directory walker runs on a background thread
  and pipes discovered files into the pipeline as soon as they're found.
  The reader pool starts pulling files before the walk finishes — no
  "sit and wait 5 minutes on cold external cache" phase.
- **Zero-copy for large files.** Files ≥ 1 MiB are `mmap`'d and passed
  as `&[u8]` directly into the compressor. No heap allocation, no
  memcpy through a `Vec`.
- **Single-alloc AES envelope.** The compressed body's `Vec` is grown
  in-place with room for salt+verify prefix and mac suffix; encryption
  happens in-place. One allocation total per encrypted file.
- **Parallel-block DEFLATE for large single-file inputs.** When the
  input is a single file ≥ 8 MiB, tzip auto-splits it into 1 MiB
  chunks, compresses them in parallel via flate2 (zlib-ng backend)
  with `Z_SYNC_FLUSH` between chunks, concatenates. Same trick as
  pigz. Turns a wall-clock single-core bottleneck into an N-core one.
- **AES-CTR counter=1 init.** Skips the dummy-block hack; first
  plaintext block uses counter=1 directly. One fewer AES op per file.
- **Hardware crypto.** `aes` and `sha1` crates dispatch to ARMv8
  crypto extensions on Apple Silicon and to AES-NI + SHA-NI on x86_64
  at runtime via `cpufeatures`. `target-cpu=native` in the release
  profile ensures the accelerated code paths are inlined.
- **`--read-jobs auto`.** Opt-in scaling to `min(cpu_jobs, 8)` when
  the source is on internal NVMe. Default stays at `2` — safe on
  external USB drives where fanning out would just stall.

### Benchmark numbers

M1 Max (10 P-cores), macOS, source and archive on internal APFS SSD:

| workload | baseline | after all opts | speedup |
|---|---|---|---|
| 500 files / 124 MB / DEFLATE default (`--read-jobs 2`) | 0.27s / 263% CPU | 0.30s / 230% | ≈ noise |
| 500 files / 124 MB / DEFLATE (`--read-jobs auto`) | 0.27s | **0.18s / 400%** | **1.5×** |
| 500 files / 124 MB / DEFLATE+AES (`--read-jobs auto`) | 0.47s / 298% | **0.24s / 588%** | **2.0×** |
| 122 MB single file / DEFLATE (parallel-block) | 1.20s / 88% | **0.33s / 540%** | **3.6×** |

Reference: `7z a -tzip -mx=9` on the same 500-file corpus completes in
~2.5s. On DEFLATE+AES with `--read-jobs auto`, tzip is **~10× faster**
than p7zip's wall time on the same hardware.

**`--raw-block` on APFS-on-USB** (M1 Max, USB SSD, large archive with
many small files):

| workload | default VFS | `--raw-block` | speedup |
|---|---|---|---|
| 79 K files / 11.7 GB → 3.3 GB / DEFLATE+AES | 2:52 | **1:26** | **2.0×** |
| 11 K files / 1.7 GB (aged data) / `-m store` | 2:15 | **0:22** | **6.1×** |
| 11 K files / 1.7 GB (fresh copy) / `-m store` | 2:22 | **0:17** | **8.2×** |

Byte-verified byte-identical to the default path via
`scripts/verify-raw-block.sh`. See
[`docs/RAW_BLOCK.md`](docs/RAW_BLOCK.md) for the architecture that
unlocked this (whole-tree metadata prefetch + sharded block cache +
parallel bulk reader with sequential extent-order preads).

### Method-throughput matrix

Same 124 MB / 500-file corpus:

| method | wall time | CPU util | output size |
|---|---|---|---|
| store | 0.59s | 18% | 129 MB |
| deflate `-x 6` | 0.31s | 229% | 51.4 MB |
| bzip2 `-x 6` | 1.71s | 728% | 51.6 MB |
| lzma `-x 6` | 1.27s | 726% | 52.0 MB |
| xz `-x 6` | 1.27s | 742% | 51.3 MB |
| zstd `-x 6` | 0.22s | 68% | 51.3 MB |
| `.7z` solid `-x 6` | 13.04s | 89% | 51.2 MB |

---

## Auto-tuned defaults

At startup tzip probes each source path and the output archive's parent
directory. If any of them lives on **APFS-over-USB** it picks the best
available combo:

- **Running as root + `raw-apfs` feature compiled:** `raw_block=on` +
  `read_jobs=8`. Uses the raw APFS parser to bypass the VFS entirely
  (2-8× faster; see benchmarks above).
- **Otherwise:** the safe VFS combo — `keep_cache=on`, `dispatch_io=on`,
  `read_jobs=4`.

If any path lives on **NTFS-on-macOS** it enables `keep_cache=on`. Any
flag you set explicitly on the command line is left alone.

On **Linux**, tzip checks the block device backing the source (via
`/sys/dev/block/<maj>:<min>`). If it's **rotational or removable** and the
filesystem supports FIEMAP, it enables the extent-order reader
(`raw_block=on`) and drops to `read_jobs=1` for a single clean forward
sweep. On NVMe the reorder is neutral, so it's left off (pass
`--raw-block` to force it). See [`docs/LINUX.md`](docs/LINUX.md).

```
$ sudo tzip out.zip /Volumes/BadDrive/src/
tzip: auto-tuned defaults for APFS-on-USB source (
    keep_cache=on, raw_block=on (root+APFS-on-USB), read_jobs=8).
[...]

$ tzip out.zip /mnt/usb-hdd/src/         # Linux, USB spinning disk
tzip: auto-tuned defaults for Linux source (
    raw_block=on (removable media), read_jobs=1).
[...]
```

Suppress the notice with `-q`. See [Known limitations](#known-limitations)
for what auto-tune does and doesn't cover.

## External-drive tuning

Design goal: never make an external drive worse. The heuristics:

- Reader pool is small by default (2 threads) so multiple threads don't
  contend for the drive's read head or command queue.
- `F_NOCACHE` (macOS) and `POSIX_FADV_DONTNEED` (Linux) applied to file
  reads by default so a multi-GB archive pass doesn't evict the user's
  working set from the page cache. **Disable with `--keep-cache` when
  the drive itself is slow — the page cache is what saves you.**
- Walker sorts entries by path before processing for directory-locality
  reads.
- Streaming walk starts the reader pool immediately — no "walk to
  completion first" phase.

### Recipes

**Internal NVMe SSD (fastest case):**
```
tzip out.zip src/ --read-jobs auto
```

**External USB with exfat filesystem (traditional external drives):**
```
tzip out.zip src/                    # defaults are already right (2 readers)
```

**External USB with APFS filesystem (see warning below):**
```
tzip out.zip src/ --keep-cache --read-jobs 1 --dispatch-io
```

**Network share (SMB, AFP, NFS):**
```
tzip out.zip /Volumes/share/src/ --read-jobs 1 --keep-cache
```

**Deep directory trees with many small files:**
```
tzip out.zip /path/to/trees/ --read-jobs auto --walk-jobs 8
# on macOS the getattrlistbulk walker + openat run automatically
```

---

## macOS-specific fast paths

Enabled by default on `target_os = "macos"`; each can be disabled with the
paired flag for debugging or fallback.

### `getattrlistbulk` bulk walker (`bulk_walker.rs`)

Instead of `readdir` + `stat` per entry, tzip calls
`getattrlistbulk(dirfd, ...)` to retrieve dozens of entries with their
attributes (name, objtype, size) in one syscall. On APFS this collapses
many B-tree lookups into a single kernel call and is what `find(1)` and
Finder use internally.

- Reduces syscall count 10-30× on wide directories
- Preserves the dirfd for the openat path
- Disabled with `--classic-walk` (falls back to jwalk)
- Currently sets mtime = DOS epoch (1980-01-01) — proper timestamp
  parsing is a TODO; use `--classic-walk` if real mtimes matter

### `openat(dirfd, basename)` reader

WorkItems from the bulk walker carry an `Arc<OwnedDirFd>` pointing at the
parent directory. The reader uses `openat(dirfd, basename, O_RDONLY)`
instead of `open(full_path)`, which skips per-component path resolution.
On deep directory trees (5-6 levels) this saves 5-6 metadata lookups
per file open.

### `F_RDADVISE` prefetch hint

After each `open`/`openat`, tzip issues
`fcntl(fd, F_RDADVISE, {ra_offset:0, ra_count:file_size})` to hint the
kernel to prefetch the whole file into the page cache. Fire-and-forget;
failures are ignored. Overlaps read latency with the mmap/compress
setup that follows.

### `dispatch_io_read` alternative reader (`--dispatch-io`)

Small files (<1 MiB) route through GCD's `dispatch_io_read` instead of
POSIX `read()`. The channel takes ownership of the fd, GCD manages the
submission to the I/O worker pool, and the shim (`src/dispatch_shim.c`)
blocks on a `dispatch_semaphore_t` until the read completes. Large files
still use mmap.

The primary win is on USB-MSC transports where GCD can pipeline more
requests to the drive than N blocking threads can. Requires macOS.

### `F_NOCACHE` page-cache bypass

Default on for `read()`-path small files. Blocks read via this fd don't
enter the unified buffer cache, so archiving 10 GB doesn't evict your
working-set data. Disable with `--keep-cache` on slow filesystems where
you'd rather benefit from readahead caching.

### `--raw-block` — raw APFS parser (`raw_apfs.rs`, feature `raw-apfs`)

Bypasses the VFS entirely: opens `/dev/rdiskN` as a block device and
parses the APFS on-disk format directly (via the vendored
[`apfs`](vendor/apfs) crate). File `open()` never happens as far as
the kernel is concerned, so on-access AV hooks
(Sophos/CrowdStrike/SentinelOne/etc.) that intercept via Endpoint
Security see nothing.

Architecture (see [`docs/RAW_BLOCK.md`](docs/RAW_BLOCK.md) for the
full story):

1. **Whole-tree metadata prefetch.** One sequential B-tree walk at
   source open, populates global `oid → InodeVal` and
   `private_id → extents` HashMaps. Every subsequent per-file lookup
   is a hashmap hit — no B-tree work per file.
2. **Sharded 64 MiB block cache** across all reader threads (16
   shards, `parking_lot::Mutex<HashMap>`) so interior B-tree nodes
   are read once and served from RAM.
3. **Bulk reader with sequential extent-order preads.** Reader threads
   split the item list into walker-order chunks, sort each chunk's
   extents by disk offset, coalesce adjacent extents, and pread each
   run in one syscall.
4. **Parallel bulk readers** (default 8 threads). Each has its own
   `ApfsVolume` handle on `/dev/rdiskN` for independent USB queue
   slots.

Access requirements: `/dev/rdisk*` is `root:operator 0640` — run
`sudo tzip …` or add yourself to the `operator` group.

Not supported: FileVault-encrypted volumes (raw reads return
ciphertext). Detected upfront; falls back cleanly to the default VFS
reader with a one-line message.

Auto-enabled when: `raw-apfs` feature compiled + macOS + APFS-on-USB
source + running as root + user didn't explicitly pass
`--raw-block=false`.

---

## Linux-specific fast paths

The compression, AES-256, SHA-1 and CRC paths already accelerate on Linux
through the same runtime CPU-feature dispatch as macOS (AES-NI + SSE4.2 on
x86_64, ARMv8 crypto on aarch64). The `posix_fadvise` /
`FALLOC_FL_KEEP_SIZE` hints in `platform.rs` are the Linux analogs of the
macOS `F_NOCACHE` / `F_PREALLOCATE` calls. On top of that Linux gets two
I/O fast paths. For picking a filesystem for an external USB drive, see
[`docs/USB_FILESYSTEMS.md`](docs/USB_FILESYSTEMS.md).

### `--raw-block` — FIEMAP extent-order reader (`linux_raw.rs`)

The Linux analog to the macOS raw-APFS reader, but no filesystem parser is
needed: the kernel already exposes each file's physical block layout via the
`FS_IOC_FIEMAP` ioctl (`fiemap.rs`). Works on ext4, xfs, btrfs and f2fs.

1. **Probe.** For each file in a window (256 files) tzip issues one FIEMAP
   ioctl to learn its first physical block offset.
2. **Sort.** The window is sorted into ascending disk order.
3. **Sweep.** Files are read in that order through the normal VFS, with
   kernel readahead issued a few files ahead so the drive stays busy.

The result is a monotonic forward sweep of the platters / flash instead of
the seek storm you get from directory-enumeration order. The payoff is on
media where seeks cost — spinning disks and USB-attached drives — exactly
where the macOS `--raw-block` path pays off. On NVMe it's roughly neutral,
so auto-tune only engages it on rotational/removable media.

Unlike the macOS raw-device path, this still reads through the VFS, so it
does **not** bypass `fanotify`-based on-access AV. Defeating enterprise AV
on Linux is a separate problem — get a per-process exclusion. See
[`docs/LINUX.md`](docs/LINUX.md).

### `--io-uring` — deep-queue async reader (`io_uring_src.rs`, feature `io-uring`)

Orthogonal to FIEMAP. The default reader issues one blocking `read()` per
reader thread, so each thread has at most one read outstanding — fine for
spinning media (the extent sweep already serializes it) but it leaves fast
NVMe idle waiting on completions. With `--io-uring` a single reader thread
submits up to 128 reads at once and reaps completions as they land, keeping
the device queue full and saturating NVMe (7 GB/s+).

It composes with `--raw-block`: the window is still sorted into physical
disk order, then submitted to the ring in that order. Requires the
`io-uring` Cargo feature and kernel ≥ 5.6.

```
cargo build --release --features io-uring
tzip out.zip /data/nvme/src --raw-block --io-uring
```

### `--apfs-device` — read an APFS volume with no mount (`raw_apfs.rs`, feature `raw-apfs`)

The same raw-APFS parser that powers macOS `--raw-block` also runs on Linux.
Because the parser reads the on-disk format itself, it needs **no mount and no
Linux APFS driver** — just read access to the device. This is the way to
archive an APFS drive (e.g. a Mac-formatted USB disk) on a Linux box that
can't mount APFS at all.

```
cargo build --release --features raw-apfs
# whole volume:
sudo tzip mac-backup.zip --apfs-device /dev/sda1
# a subtree (in-volume paths):
sudo tzip subset.zip --apfs-device /dev/sda1 /Users/me/Documents
```

- **Whole-volume walk + read come from the parser** (catalog B-tree +
  extents), so directory enumeration works without a mount.
- **FIEMAP does not apply here** — FIEMAP needs a mounted filesystem whose
  kernel driver services the ioctl; an unmounted device (or one mounted via
  read-only apfs-fuse, which doesn't implement fiemap) has nothing to answer
  it. The raw parser is the right tool, and it's also faster (one sequential
  catalog scan up front, then hashmap lookups per file).
- **Access:** the device node is `root:disk 0660`. Run under `sudo`, join the
  `disk` group (`sudo usermod -aG disk $USER`, persistent), or grant a
  temporary ACL (`sudo setfacl -m u:$USER:r /dev/sda1` — note udev can reset
  this when the node is re-created).
- **Not supported:** FileVault / block-layer-encrypted volumes — catalog pages
  are ciphertext at the block layer, so parsing fails up front with a clear
  message.

---

## APFS-on-USB

**tl;dr:** APFS-on-USB *used* to be catastrophically slow for
archivers, because APFS's small-random-metadata pattern hits the
per-command USB-MSC round-trip latency on every B-tree read. With
`--raw-block` (auto-enabled under sudo on macOS builds with the
`raw-apfs` feature), tzip **beats the VFS path by 2-8×** on the same
drive by pre-reading the whole catalog B-tree in one sequential
pass and doing all subsequent lookups from RAM — see the benchmarks
above.

If you can't use `--raw-block` (no sudo, or on FileVault-encrypted
volumes), the safe fallback is auto-tuned by tzip:
`--keep-cache --read-jobs 4 --dispatch-io`. This takes the edge off
but doesn't match `--raw-block` speed.

The classical failure mode when neither is available:
- `dd if=<file> of=/dev/null bs=1m count=200` on an APFS-USB volume
  runs at 3-10 MB/s where exfat on the same hardware runs at
  50-150 MB/s
- `readdir` of a directory with a few thousand files takes 30+ seconds
- Any archiver (p7zip, ditto, WinZip) appears "hung"; the archiver
  isn't the bottleneck, the driver stack + APFS metadata pattern is

Manual override on the fallback path:

```
tzip out.zip src/ --keep-cache --read-jobs 1 --dispatch-io
```

### If you can reformat

Before `--raw-block` existed, we'd have suggested exfat here. With
`--raw-block` in place, staying on APFS is now the better choice on
macOS. Only reformat if you also need to use the drive from
Windows / older systems that can't read APFS.

For Windows-compatibility case: Disk Utility → *physical disk* →
Erase → Format `ExFAT` → Allocation Unit Size `32 KB`. GUID Partition
Map. Then rsync your data back.

---

## Known limitations

- **`.7z` solid is single-threaded** — sevenz-rust 0.6 does not expose
  multi-threaded LZMA2. Use `-m xz` inside a `.zip` for parallelism.
- **Bulk walker sets mtime = DOS epoch** — the `getattrlistbulk`
  attribute-alignment parser doesn't currently decode `ATTR_CMN_MODTIME`
  correctly; the field is skipped and mtime is set to 1980-01-01. Use
  `--classic-walk` if real mtimes matter. TODO.
- **Symlinks are not preserved as links** — the ZIP writer has no
  symlink-entry support, so a symbolic link is archived as a *regular
  file whose contents are the link target path* rather than a symlink.
  On extraction you get a small regular file, not a link. This is most
  visible on Unix source trees (and APFS volumes via `--apfs-device`),
  where symlinks are common. Storing proper symlink entries (unix mode
  `S_IFLNK` in the external-attributes field) is a planned follow-up.
- **Cancel is cooperative** — TUI `q` sets a flag; the pipeline
  finishes in-flight work before exiting.
- **`--sort` + AES is not byte-identical** across runs — the random
  salt per entry differs. Deterministic AES output would require
  keying the salt from a stable hash.
- **Windows build is untested** — the macOS fast paths are compiled
  out, but the Windows read path itself hasn't been exercised on a
  real Windows host.
- **No streaming input** — the writer needs a seekable output file
  (patches the local file header sizes; ZIP data descriptors are
  planned but not yet wired via `--streaming`).

---

## Architecture

```
                    ┌──────────────────┐
                    │  Walker          │  jwalk OR getattrlistbulk (macOS)
                    │  (bg thread)     │  streams WorkItems as discovered
                    └────────┬─────────┘
                             │  crossbeam-channel
                             ▼
                    ┌──────────────────┐
                    │  Reader pool     │  default 2 threads
                    │  LocalFsSource   │  posix read | mmap | dispatch_io
                    │  or Dispatch...  │
                    └────────┬─────────┘
                             │  bounded channel (2 × read_jobs)
                             ▼
                    ┌──────────────────┐
                    │  CPU pool        │  N = physical cores
                    │  compress +      │  libdeflater / xz2 / bzip2 / zstd
                    │  AES-256 encrypt │  per-worker Scratch (reused bufs)
                    └────────┬─────────┘
                             │  bounded channel (2 × cpu_jobs)
                             ▼
                    ┌──────────────────┐
                    │  Writer thread   │  single; hand-rolled ZIP writer
                    │  LFH + CDR +     │  ZIP64 fallback; AE-2 extra field
                    │  EOCD (+ ZIP64)  │  UTF-8 name bit; Unix mode
                    └──────────────────┘
```

### Module map

```
src/
├── main.rs             CLI entrypoint
├── cli.rs              clap args + Options struct
├── walker.rs           jwalk-based streaming walker (portable)
├── bulk_walker.rs      macOS getattrlistbulk walker (fast path)
├── platform.rs         OwnedDirFd, F_NOCACHE/F_RDADVISE/F_RDAHEAD,
│                       mmap-or-read, POSIX_FADV_DONTNEED (Linux)
├── pipeline.rs         reader/CPU/writer orchestration + Source trait
├── dispatch_io.rs      GCD dispatch_io_read Source (macOS opt-in)
├── dispatch_shim.c     C shim with Objective-C blocks (built by build.rs)
├── compress.rs         methods + Scratch (buffer pool) + parallel-block DEFLATE
├── crypto.rs           WinZIP AES-256 AE-2 in-place encrypt
├── zipwriter.rs        hand-rolled LFH/CDR/EOCD/ZIP64/AES-extra-field
├── sevenz.rs           .7z solid via sevenz-rust
├── progress.rs         indicatif progress bar (determinate + spinner)
└── tui.rs              ratatui dashboard
```

### `Source` trait

The read path is abstracted:

```rust
pub trait Source: Send + Sync {
    fn read(&self, item: &WorkItem) -> Result<platform::ReadBuf>;
}
```

Current implementations: `LocalFsSource`, `DispatchIoSource`. This is the
seam where future cloud backends (S3, Google Drive, Box) plug in —
they'll stream from an HTTP body into a `Bytes`-like buffer without
touching the local filesystem.

---

## Building from source

Standard:

```
cargo build --release
```

The release profile in `Cargo.toml`:
- `opt-level = 3`
- `lto = "thin"`
- `codegen-units = 1`
- `panic = "abort"`
- `strip = "symbols"`

`.cargo/config.toml` sets `-C target-cpu=native` so hardware crypto
instructions (ARMv8 SHA1/AES, x86_64 SHA-NI/AES-NI/AVX2) inline into the
build. Rebuild after moving to a different CPU generation.

macOS-only artifacts:
- `build.rs` compiles `src/dispatch_shim.c` with `-fblocks` and links
  `libSystem` (blocks + libdispatch are built in on Apple)
- `bulk_walker` compiled only on `target_os = "macos"`

Cross-compilation targets have not been exercised. Contributions welcome.

---

## Testing

```
cargo test                    # debug build tests
cargo test --release          # release build tests (recommended pre-commit)
```

Three integration suites:

- `tests/roundtrip.rs` — create-and-extract round-trips for STORE,
  DEFLATE (levels 0 and 12), BZIP2, LZMA, XZ, Zstd (via system `7z`),
  AES-256 structural check, `.7z` solid (plain + AES), `--sort`
  byte-identical determinism.
- `tests/update.rs` — `tzip u` in both append and rebuild modes: add,
  replace, delete, mixed, AES preservation across kept entries,
  `--sort`, create-if-missing, `.7z` rejection, stale-undo cleanup,
  crash recovery from a fabricated truncated archive.
- `tests/reader.rs` — `tzip ls`, `tzip cat` per compression method
  (store / deflate / bzip2 / lzma / xz / zstd), AES cat with correct /
  missing / wrong password, whole-archive and selective `tzip x`,
  zip-slip rejection, create-then-extract identity.

43 integration tests total. CI runs them on ubuntu-latest and
macos-latest (see `.github/workflows/ci.yml`).

---

## Attribution & wire-format references

- PKWARE APPNOTE.TXT v6.3.10 (ZIP format)
- WinZIP AES specification (`0x9901` extra field, AE-2 variant)
- `getattrlistbulk(2)` — Apple developer documentation
- Grand Central Dispatch `dispatch_io_read` — Apple developer documentation
- pigz — reference for parallel-block DEFLATE with `Z_SYNC_FLUSH`

---

## License

BSD-3-Clause. See [LICENSE](LICENSE).
