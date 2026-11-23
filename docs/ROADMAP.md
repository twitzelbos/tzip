# tzip roadmap

Living document. Captures the direction the project could take,
with honest scope + effort estimates + build-vs-buy analysis for
each phase.

**Current state (2026-11-23).** tzip is a fast parallel ZIP/7z
creator with a macOS `--raw-block` fast path that hits 2-8× the
default VFS throughput on APFS-on-USB, a Linux FIEMAP / io_uring
reader path, and a Ratatui `--tui` dashboard. It now also:

- **Updates archives in place** — `tzip u` adds / replaces / deletes
  entries without re-compressing or re-decrypting the kept bodies.
  Picks between an **append** path (truncate at old CDR, append in
  place, zero-rewrite — crash-safe via an on-disk undo file with
  auto-recovery) and a **rebuild** path (byte-copy kept entries into
  a temp, atomic rename) based on whether the op is pure-add.
- **Reads what it writes.** `tzip ls` / `tzip cat` / `tzip x` cover
  every method tzip can produce (store / deflate / bzip2 / lzma / xz
  / zstd) plus WinZIP AES-256 AE-2, with streaming decrypt +
  decompress, constant-time password verification before any
  plaintext ships, HMAC verification, and a zip-slip guard on
  extract.
- **Subcommand CLI** (v0.2). `tzip <archive> <paths>` → `tzip create
  <archive> <paths>`. Reader subcommands land at the same time.
- **CI + branch protection.** GitHub Actions matrix (ubuntu + macos)
  runs all three integration suites (43 tests); branch ruleset on
  `main` requires PRs and both CI jobs to pass.

**Guiding constraints for the next phases.**

- **ZIP-as-a-filesystem is the new near-term direction.** The
  reader stack (Phase G below) extends tzip past "archive creator"
  toward "mountable, grep-able, cat-able live view of any ZIP —
  local, HTTP, S3." This ladders naturally and ships value at every
  increment (local reader is already live; remote and mount are
  queued).
- **S3-backed storage is still the strategic end state** (Phases
  C → D → E / F below), but the reader stack is a strictly
  smaller, faster-landing path to the same "read an archive in S3"
  outcome without committing to a new on-disk format.
- **Target platforms:** macOS 15+ (FSKit-based mount is fine) and
  modern Linux. Windows deferred.
- **Every phase must be measurable against JuiceFS** as the honest
  baseline for the S3-backed compressed storage use case.

---

## Phase A — Linux platform port

**Status (2026-09-30): largely landed.** The FIEMAP extent-order reader
(`--raw-block` on Linux), the io_uring deep-queue reader (`--io-uring`,
feature `io-uring`), Linux auto-tune (rotational/removable detection via
`/sys/dev/block`), and `statfs`-magic filesystem naming are implemented and
byte-verified against the default reader. See [`LINUX.md`](LINUX.md).

The raw-APFS parser is also now **ported to Linux** (`--apfs-device
/dev/sdXN`, feature `raw-apfs`): archive an APFS volume by parsing it off a
raw block device with no mount and no APFS driver. Verified against a 931 GB
Mac USB disk (whole-catalog walk, 11,499/11,499 DICOM files byte-valid on
round-trip). This is the Linux answer for APFS media — FIEMAP can't help an
unmounted volume. All reader backends now share one `Source::read_bulk`
bulk path.

Still open: a `getdents64`/`statx` bulk walker and `O_DIRECT` cross-file
extent coalescing.

**Goal.** Give Linux users the same architecture wins we've built for
macOS. Same tzip binary, same CLI, same TUI.

**Direct wins (no work):**
- Compression + AES + SHA already accelerate on Linux via the same
  `.cargo/config.toml` cfg flags (x86_64 gets AES-NI + SSE4.2,
  aarch64 gets ARMv8 crypto).
- The pipeline shape is portable.
- `--basename-only` / `zip -r` path convention is portable.

**Needs a Linux equivalent (mechanical port, small):**
- `F_NOCACHE` / `F_RDADVISE` / `F_PREALLOCATE` → `posix_fadvise`,
  `readahead(2)`, `fallocate(2)`. `platform.rs` already has
  conditionally-compiled paths.
- `getattrlistbulk` bulk walker → `getdents64` + `statx(AT_STATX_DONT_SYNC)`.
  jwalk is already the current fallback and is fine as a starting
  point.
- `openat(dirfd, basename)` — already portable, we use `libc::openat`.

**The Linux-specific big win — analog to `--raw-block`:**
- **`FIEMAP` (`FS_IOC_FIEMAP` ioctl)** — returns a file's physical
  block layout on ext4 / xfs / btrfs / f2fs without parsing the
  filesystem. Gather every file's extents, sort by disk offset,
  coalesced sequential preads via the VFS. Same architectural win
  as `--raw-block` on macOS, and it works on every major Linux
  filesystem. **No custom filesystem parser needed** because the
  kernel exposes the layout via a standard ioctl. This is the
  biggest Linux ROI.
- **`io_uring`** — orthogonal to FIEMAP. Async I/O with queue
  depth 128+, lets a single thread saturate NVMe (7 GB/s+).
  Feature-flagged (`io-uring` crate is stable). Kernel ≥ 5.11.
- **`O_DIRECT`** — analog of raw-device bypass for the page cache.
  Sometimes helps against AV that only hooks the page-cache path.

**AV bypass on Linux is a different problem:**
- Linux enterprise AV (Sophos Linux, CrowdStrike Falcon) hooks via
  `fanotify` + `LD_PRELOAD` shims, not raw device access.
  `--raw-block`-style tricks don't equivalently apply.
- Best answer stays "get a per-process exclusion."

**Suggested order:**
1. ✅ Baseline the current binary on the target Linux machine — probably
   already fine on plain NVMe. Establish numbers to beat.
2. ✅ `posix_fadvise` / `readahead` hints — already in `platform.rs`;
   the extent reader adds forward-looking `readahead(2)`.
3. ✅ FIEMAP-based extent-order reader — `src/linux_raw.rs` +
   `src/fiemap.rs`. Same shape as `RawApfsSource` but no filesystem
   parser; works across ext4/xfs/btrfs/f2fs.
4. ✅ `io_uring` reader — `src/io_uring_src.rs`, feature `io-uring`.
   Composes with the extent reader.

**Estimated effort:** 2-4 weeks total. Payoff: on par with
`--raw-block` speedups where AV pressure exists; substantial
speedups on NVMe from `io_uring` regardless of AV.

---

## Phase B — `tzip compact` for APFS transparent compression

**Goal.** Give macOS users transparent per-file compression using
APFS's existing `com.apple.decmpfs` mechanism. Every app, Finder
included, sees uncompressed data; the drive stores compressed. R/W,
zero mount, zero KEXT.

**How it works today.** APFS ships `decmpfs` support in the kernel.
`afsctool` (Homebrew) walks a directory tree compressing files in
place via decmpfs xattr. Reads decompress in the kernel. Very effective
on unstructured data, works with everything.

**Limitations we inherit:**
- Codecs are Apple's: LZFSE, LZVN, ZLIB, LZBITMAP. No zstd/xz.
- Max 4 GiB per file for decmpfs. Larger files stay uncompressed.
- APFS-only (not exFAT / NTFS-formatted external drives).

**What tzip adds over `afsctool`:**
- Parallelized bulk compression using the existing worker pool.
  `afsctool` is single-threaded and old. Expected: 5-10× faster on
  M-series machines.
- Same CLI ergonomics as `tzip` create — `tzip compact <path>` +
  auto-tune for the drive.
- Optional `--watch <dir>` daemon that compresses newly-added files.
- `tzip compact --stats <dir>` audits ratio + savings.

**User outcome:**
- 30-70% capacity increase on typical data.
- Effective read bandwidth from mechanical / slower USB drives
  multiplied by compression ratio (kernel decompresses in RAM).
- Transparent to every app.

**Estimated effort:** 3-5 days. Reuses walker, compressor pool,
progress infrastructure. Main new work: decmpfs xattr writer +
per-codec byte-format wrappers.

---

## Phase C — Chunked local compressed container (`.tpk`)

**Goal.** A single-file, R/W, random-access compressed container for
the cases decmpfs can't handle (exFAT drives, files > 4 GiB, need
for modern codecs).

**Container shape:**

```
[Superblock] [Chunk Index] [File Table] [Chunk Store …]
                                        ^ grows
```

- **Fixed-size chunks** (e.g. 64 KiB uncompressed), each independently
  compressed with tzip's existing codecs (`deflate`, `zstd`, `xz`,
  `lzma`, `bzip2`). Independent chunks → random-access reads.
- **Chunk Index:** `chunk_id → (offset, compressed_size, ref_count)`.
- **File Table:** `path → (size, mtime, mode, [chunk_id, …])`.
- **Superblock:** magic, version, chunk size, algo, root pointer.

**Operations:**
- **Reads.** File table lookup → for each chunk, seek + decompress.
  O(1) per chunk after cache warm.
- **Writes.** Compress new chunk, allocate slot (tail-append or reuse
  a ref-count-0 slot), update file table, atomic-swap the superblock's
  root pointer.

**Free bonuses that fall out of chunking:**

1. **Content-addressed dedup.** Hash each chunk before writing; if
   the hash exists, reuse the existing chunk (bump ref-count) instead
   of storing again. Massive win on similar-file corpora (e.g. medical
   imaging series, backup snapshots, source trees with shared
   headers). Same trick Restic / Borg / rdedup use.
2. **Cheap snapshots.** Chunk Index and File Table are just data
   structures. Snapshotting = copy the File Table.

**Concurrency.** Single writer (mutex or lockfile) + concurrent
readers to start. Later: per-chunk locking for concurrent writers.

**Crash safety.** Write new File Table at end, atomic-swap the
superblock's file-table pointer + fsync. Old File Table stays for
one generation as rollback.

**CLI (no mount):**
- `tzip pack add <container> <files...>`
- `tzip pack ls <container> [path]`
- `tzip pack cat <container> <path>`
- `tzip pack rm <container> <path>`
- `tzip pack stats <container>`
- `tzip pack gc <container>` — compact chunk store, drop ref-0 chunks

**Estimated effort:** 1-2 weeks. On-disk format doc first, then
implementation, then dedup, then GC.

**Trade-off vs `tzip compact` (Phase B):**
- Phase B is transparent to all shell tools but limited to APFS +
  Apple codecs + 4 GiB files.
- Phase C works anywhere but requires tool-mediated access
  (`tzip pack cat` instead of `cat`).

---

## Phase D — S3 backend for the chunked container

**Goal.** Same chunked-container format, backed by an S3 bucket.
Bandwidth win: pull only the chunks a file needs, not the whole
container. Space win: dedup + compression at rest.

**S3-native constraints that shape the design:**
- Objects are immutable — no in-place edits. Every "write" is a
  full-object PUT.
- Per-request cost (~$0.0004 per 1000 GETs, higher for PUTs).
- Range GETs are cheap and fast.

**Layout — Restic/Borg style pack layout:**

```
s3://bucket/tzip-repo/
    config                          # small: codec, chunk size, version
    index/000001.idx                # chunk_id → (pack_id, offset)
    index/000002.idx                # newer generations appended
    snapshots/2026-09-29T12.snap    # file table (path → [chunk_id])
    pack/xx/xxxxx.pack              # ~4-8 MiB blobs, holding many chunks
```

- **Packs** bundle many chunks per S3 object → amortizes per-request
  cost and minimum-billed-size (128 KB for Glacier/IA).
- **Writes** = new packs + new index + new snapshot. Old packs stay
  until GC. Nothing ever mutated.
- **Reads** for a specific file = pull the snapshot (small) + the
  index (cached, small) + Range GET only the chunks that file needs.

**Bandwidth model.** User accesses one 50 KB file out of a 1 TB
backup → downloads ~50 KB + metadata, not 1 TB. Massive vs. "download
the whole zip."

**This is exactly what Restic and Borg do.** Building it inside
tzip means racing mature projects on ops experience. See Phase F
for the "just use Restic" alternative.

**Estimated effort:** 2-4 weeks (S3 auth, retry/backoff, index
caching, GC, snapshots, robustness). Multiplies rapidly with
edge-case testing.

---

## Phase E — Transparent FUSE / FSKit / WinFsp mount

**Goal.** Mount a `.tpk` container (local or S3-backed) as a real
filesystem. `cat`, `grep`, `vim`, `find`, `git`, and every other
shell tool operate on it as if it were a normal folder.

**What this actually requires:**
- **FUSE / FSKit / WinFsp driver** — kernel calls our code for
  `open`, `read`, `write`, `getattr`, `readdir`, `create`, `unlink`,
  `rename`, `truncate`, `chmod`, `mmap`, `flock`, `xattr`, `symlink`,
  `hardlink`, `sync/fsync`, etc.
- **Full POSIX semantics** — `mmap` works, hardlinks work, atomic
  renames work, sparse files work, `find -inum` returns stable inode
  numbers, `git` doesn't get confused, `vim`'s swap-file rename
  pattern works.
- **Concurrent access** — two shells at once shouldn't corrupt.
- **Crash safety** — kill -9 the mount daemon and remount should
  give a consistent view.
- **Write buffering** for S3-backed containers (immutable object
  store + block-level writes don't natively align).

**Platform mapping** (project targets macOS 15+ and modern Linux):
- **macOS 15+.** FSKit — Apple's supported user-space filesystem
  framework. No KEXT, no Reduced Security prompts, no macFUSE
  dependency. Clean and future-proof. Rust bindings are early but
  FSKit's Swift/ObjC interface is stable.
- **Linux.** FUSE3 is stable and clean. ~2 weeks read-only mount,
  4-8 weeks R/W.
- **Windows.** WinFsp works, adds a dependency to ship. Punt until
  there's demand.

**Estimated effort:** Read-only mount ~2 weeks per platform. R/W
mount plus S3 write-buffering: **multiple months** for robustness
testing (POSIX edge cases: mmap, hardlinks, atomic rename patterns
that `git`/`vim` rely on, crash consistency).

**Honest assessment.** The container + backend work (Phases C/D)
is bounded. The mount work is where user-space filesystems
traditionally spend the majority of their time on edge cases.

---

## Phase F — JuiceFS integration (as an alternative to D + E)

**JuiceFS** ([juicefs.io](https://juicefs.io)) is Apache-2.0
open-source, multi-platform (Linux + macOS FUSE + Windows WinFsp),
with S3 (or any object store) backend, chunk-based dedup, optional
zstd/LZ4 compression, snapshots, and mounts as a POSIX filesystem
so every shell tool works. Metadata in Redis / SQLite / PostgreSQL.
Actively developed, ~10K GitHub stars, production-deployed at scale.

**That's the full spec of Phase D + Phase E, shipped, hardened,
and free.** Building it from scratch inside tzip would duplicate
years of JuiceFS engineering.

**What tzip could add on top:**
- `tzip compact --to-juicefs <mount>` — pipe archive stream directly
  into a JuiceFS-mounted directory with compression applied.
- macOS-native UX polish around JuiceFS (auto-mount, status TUI,
  simplified S3 config).
- Same TUI dashboard hooked into JuiceFS metrics.

**Estimated effort:** ~1 week. Almost all of it is UI wrapping.

**When to prefer F over D+E:**
- S3 + transparent-mount is the top priority.
- Not enough team-weeks to spend on a full FUSE + S3 stack.
- Willingness to ship "tzip + JuiceFS" as a stack, not tzip alone.

**When to prefer D+E over F:**
- Standalone tzip that owns the whole stack matters strategically.
- The ability to iterate on the container format without JuiceFS's
  constraints matters.
- Team has multiple months of runway for it.

---

## Phase G — ZIP as a readable / mountable filesystem

**Pitch.** The elevation-past-7-zip play. 7-zip (and every other
archiver) treats a ZIP as a container you *extract from*. tzip treats
it as a **live filesystem you can browse, grep, cat, and mount —
local or remote**. The ZIP spec cooperates: the CDR lives at the end
of the file and every entry's LFH + body is independent, so a Range
GET against a ZIP in S3 can pull one 10 KB file out of a 100 GB
archive without staging anything locally.

Works on every existing ZIP ever written. **No new on-disk format
needed.** Composes orthogonally with the C→D→E `.tpk` + S3 + mount
path: that story is "tzip owns the container"; this story is "tzip
makes *any* ZIP behave like a modern cloud object store."

Ladders in four increments, each shippable on its own.

### Increment 1 — local reader (SHIPPED, PR #4)

`tzip ls`, `tzip cat`, `tzip x` over a local `.zip`. Streaming
decrypt + decompress for every method tzip can write. Zip-slip
rejection on extract. 18 integration tests.

### Increment 2 — HTTP(S) Range source (~1 week)

`tzip ls https://host/archive.zip`, `tzip cat https://…
path/to/file.txt`. Adds a feature-flagged dependency (`ureq` or
`reqwest`). Three Range GETs per `cat`: last 64 KiB for the EOCD,
then the CDR, then the LFH + body for the target entry. The
per-entry pipeline (zipreader → crypto::decrypt_ae2 → decompress)
is unchanged — only the `Read + Seek` source swaps.

### Increment 3 — S3 source (~1-2 weeks)

`tzip ls s3://bucket/key.zip`, same for cat / x. Auth via env /
profile / instance role. Bounded retries + 429/503 backoff. The
`Seek` abstraction over an S3 object uses Range GETs identically to
Increment 2; the delta is auth and operational robustness.

### Increment 4 — read-only mount (~2-4 weeks per platform)

`tzip mount archive.zip /mnt/arch` makes every shell tool — `cat`,
`grep`, `vim`, `find`, `git` — work against the archive as if it
were a regular directory. FSKit on macOS 15+, FUSE3 on Linux. R/O
first; R/W is deliberately deferred (see "next killer" below).

**Access to a 1 TB S3-hosted archive, after Increments 2-4**:
`mount` at ~instant (only metadata fetched), `ls` at RAM speed,
`cat` at S3-Range latency, `grep` at the speed of the matching
subset (not the whole archive).

**Combined estimate for Increments 2-4**: ~1-2 months of focused
work. Ships value after every increment.

---

## Recommended ordering

**Primary constraint (per project owner):**
- **S3 layer is the most critical.** Goal: minimum S3 storage
  billed, maximum effective bandwidth (only highly-compressed data
  crosses the internet).
- **Target platform:** macOS 15+ (FSKit-based mount is acceptable)
  and modern Linux.

The shipped work (update mode + Phase G Increment 1) revealed a
shorter path to the "read what's in S3 without downloading the whole
thing" outcome than the full C→D→E rebuild requires. Updated ordering:

1. **Phase G Increments 2-3 (HTTP + S3 reader).** ~2-3 weeks total.
   Delivers the primary S3 goal — cat and extract one file out of a
   remote ZIP without staging — without any new container format.
   Works on any ZIP anyone else has ever produced. Reuses the local
   reader pipeline shipped in Increment 1.
2. **Phase G Increment 4 (read-only mount).** ~2-4 weeks per
   platform. Turns the reader into a shell-transparent view of any
   ZIP, local or remote. R/W deferred.
3. **Phase B (`tzip compact`).** ~3-5 days. Orthogonal local-APFS
   quick win; can slot in any time.
4. **Phase C (`.tpk` chunked container)** — still prerequisite for D
   if the dedup story matters. Reconsider after Increments 2-3 ship:
   at that point we'll know whether S3 with per-ZIP Range reads is
   already enough (likely for Qbio-style workloads with few archive
   versions) or whether dedup across archives is load-bearing.
5. **Phase D (S3 backend for `.tpk`)** — depends on C. Build iff the
   cross-archive dedup savings justify a new format vs. just dropping
   many ZIPs in S3.
6. **Phase H candidates (see below)** — the "next killer after Phase
   G" question.
7. **Phase A (Linux platform port)** — SHIPPED (FIEMAP + io_uring +
   Linux auto-tune, per Phase A section above).

---

## Phase H — candidates for the next step-change after Phase G

Once Phase G lands end-to-end (local + HTTP + S3 + read-only mount),
two credible step-change features are on deck. Pick one; they aren't
mutually exclusive but they pull the project in different directions.

### Phase H1 — `tzip grep` / `tzip find` across many archives (recommended)

**Pitch.** Fan the reader pipeline out across many ZIPs. `tzip grep
'PANIC' s3://bucket/logs-*.zip` opens each archive's CDR via a Range
GET, filters entries by name/size/mtime, streams-decrypts-decompresses
only the matching bodies, and greps them in parallel. Same for `tzip
find`. The outcome is "search 500 nightly backup archives totalling
10 TB in S3, get hits in seconds, download nothing you don't match."

**Why it matters.** No other archiver does this. `aws s3 cp *.zip .
&& 7z x *.zip && grep …` is hours and terabytes; `tzip grep` is
seconds and megabytes. It's the most direct way to turn Phase G's
"random-access ZIP reader" into a clear replacement for an entire
ops workflow.

#### Required capabilities (not optional)

- **Controllable parallelism, three independent levels.** Massive S3
  fan-outs need dialable concurrency — hammer too hard and you hit
  per-prefix 503s; too soft and you leave bandwidth on the floor.
  - `-P, --parallel <N>` — concurrent *archives* in flight. Default
    ~32; cap is the fan-out across the archive set.
  - `--inner-jobs <N>` — concurrent *entries* within one archive.
    Default ~8; this is the fan-out *inside* a single ZIP so a
    large archive doesn't serialize behind one entry at a time.
  - `--connections <N>` — shared HTTP(S)/S3 connection pool cap.
    Default = `parallel × inner_jobs` capped at a sane ceiling
    (e.g., 256) to keep TLS handshakes bounded independently of
    the two fan-out knobs.
  - All three overridable; tzip prints the effective values under
    `-v`. Defaults are tuned for S3's documented per-prefix soft
    limits (~5,500 GET/s per prefix historically) and typical
    bucket layouts.

- **Resumable long operations (`--checkpoint` + `--resume`).** A
  multi-hour grep over 10 TB of S3 archives cannot afford to start
  over if it's interrupted (Ctrl-C, OOM, spot-instance reclaim,
  network blip, the operator's laptop closing). tzip persists
  progress to a small on-disk checkpoint and can pick up where it
  left off.
  - `--checkpoint <FILE>` — JSONL file atomically updated as each
    `(archive, entry)` completes: `{archive, entry_name, hits,
    bytes_read, ts}`. One line per completed entry; fsync on each
    flush batch (tunable). No central lock — each worker appends
    to its own shard which is merged at the end.
  - `--resume` — on next invocation with the same args + checkpoint,
    skip every `(archive, entry)` already present and emit only
    new matches. Output dedup is implicit: a match that landed in
    the checkpoint will not be re-emitted.
  - **Granularity is per-entry, not per-byte.** An entry interrupted
    mid-stream restarts at byte 0 of that entry on resume — doing
    byte-offset resume would require either stable deterministic
    match positions across decompression (hard) or
    decompressor-restart markers (format-specific). Per-entry
    granularity is cheap and sufficient: the worst-case extra work
    is one in-flight entry per worker, repeated. For grep-style
    ops over log archives, that's negligible.
  - Checkpoint is self-describing: it records the invocation's
    pattern, archive-set glob, and filter flags. `--resume`
    refuses to run against a checkpoint written by a different
    invocation (prevents accidentally resuming the wrong query).
  - Crash safety: the checkpoint is append-only JSONL with atomic
    per-line writes; a half-written line is ignored on parse.
  - Optional `--checkpoint-every <N>` to flush less aggressively
    for tiny-entry corpora where per-entry fsync dominates.

- **Entry filtering at the CDR.** `--include GLOB`, `--exclude GLOB`
  (both repeatable), `--max-size`, `--min-size`, `--since MTIME`,
  `--until MTIME`. All applied at the CDR, so entries that don't
  match never trigger a body fetch — big win on S3 where every
  skipped entry is a saved Range GET.

- **Output format.** Default: `archive.zip:entry/path:line-no:match`
  (grep-style, colorized when stdout is a TTY). `--json` for
  structured consumption. Line-buffered so a long run can be piped
  into `head` without full-buffer deadlock.

**Scope.** Builds directly on Phase G Increments 2-3 (HTTP + S3
readers). ~2-3 weeks including the parallelism + resume work (the
original ~1-2 week estimate was for the naive fan-out only).

**Tradeoff.** Narrower than mount R/W. Doesn't change the "tzip is
a filesystem" story — just makes the reader fan out. But it's a
clean win that ships fast and composes with everything in Phase G.

### Phase H2 — Read/write mount

**Pitch.** Promote Phase G Increment 4 from R/O to R/W. Edit a file
in the mounted archive; tzip batches the dirty entries and commits
them to the underlying ZIP via the existing `tzip u` machinery
(append when safe, rebuild otherwise). Combined with S3 (Phase G
Increment 3), this is "mount a 100 GB S3-hosted ZIP, `vim` a config
file inside it, save, the archive updates in place."

**Why it matters.** This is the full "tzip is a live filesystem,
not an archive" positioning. Once R/W works, the archive stops
being a container and starts being an addressable, encrypted,
compressed filesystem — a step-change in mental model that nothing
else in the archiver space offers.

**Scope.** Multi-month, per existing Phase E notes. The hard part
is POSIX edge-case stability under R/W: vim swap-file rename
patterns, git's atomic-rename-over-target, mmap, hardlinks /
symlinks, inode-number stability, crash consistency on the ZIP
tail, concurrent writers. The archive-side primitives are already
there (append + rebuild + undo recovery); the mount-side plumbing
is what takes the time.

**Tradeoff.** Biggest payoff, biggest cost. Needs a product
commitment to "tzip is a filesystem," not just an archiver.

### Which to pick

- **Pick H1 if** you want the next ship-and-measure win on the
  critical path and are willing to defer R/W mount to a later cycle.
  Matches the lesson from the reader-stack pivot: there's a shorter
  path to the primary outcome.
- **Pick H2 if** you're ready to commit multi-month runway to the
  full filesystem story and the R/W mount's mental-model shift is
  worth more than any amount of grep speed.

If unsure, ship H1 first — it's small, it's measurable, and it
makes H2 easier to justify later.

**Critical check before committing to C→D→E: benchmark against
JuiceFS (Phase F).** If JuiceFS's storage ratio + bandwidth
efficiency is within ~10-20 % of what we'd hand-build, integrating
JuiceFS ships months faster with the same user outcome. Only reason
to build C→D→E from scratch is if:
- We measure JuiceFS's compression / dedup being materially worse
  than what tzip's codecs + chunk size give us.
- We want single-binary distribution (no external metadata store).
- We want to own the container format for other reasons (e.g.
  cross-tool interop with tzip archives).

**Concrete first step in either path:** benchmark JuiceFS with zstd
compression on the actual target workload — measure (a) S3 storage
size, (b) bandwidth per representative access pattern, (c) mount
setup complexity. Use those numbers as the target for a from-scratch
build to beat. If it beats JuiceFS by <10 %, integrate JuiceFS; if
it beats by >30 %, build.

---

## Non-goals (worth being explicit about)

- **Reinventing Restic/Borg for its own sake.** They solve the
  backup-to-S3 problem well. tzip's differentiator should be
  something they don't do (e.g. wall-clock speed of the initial
  archive creation, `--raw-block`, TUI polish).
- **Cross-platform FUSE from day one.** Ship one platform at a
  time; Linux first because FUSE3 is painless there.
- **General-purpose distributed filesystem.** JuiceFS's territory.
- **AV bypass via anything more invasive than raw device reads.**
  `--raw-block` already tests the limits of what's acceptable.
  Anything further crosses into malware-adjacent territory.

---

## Open questions

- **JuiceFS baseline.** What are its actual numbers on the target
  workload — S3 storage size after dedup, bandwidth per typical
  access pattern, mount setup? Blocks the C→D→E vs F decision.
- **S3 access patterns.** Backup-and-restore (write-heavy, occasional
  read of a single file) vs read-heavy (many small file fetches from
  cold storage) shape chunk-size and pack-size defaults very
  differently. What's the workload?
- **Metadata store for Phase D.** Restic keeps the index in S3
  itself. JuiceFS uses an external metadata service (Redis / Postgres
  / SQLite). Different tradeoffs on latency vs single-binary
  simplicity.
- **Positioning.** Is tzip "the fastest archive creator" or "a
  compressed storage stack"? Those want different investment
  profiles.
- **Backward compat.** Once `.tpk` ships, it's a format we support
  forever. Spec discipline matters early.
