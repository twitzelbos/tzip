# tzip roadmap

Living document. Captures the direction the project could take,
with honest scope + effort estimates + build-vs-buy analysis for
each phase.

**Current state (2026-09-29).** tzip is a fast parallel ZIP/7z
creator with a macOS `--raw-block` fast path that hits 2-8× the
default VFS throughput on APFS-on-USB. Ratatui `--tui` dashboard
with throughput sparkline. `zip -r`-style archive paths.
Byte-verified interop with 7-zip.

**Guiding constraints for the next phases.**

- **The S3 layer is the top priority.** Goal: minimum S3 storage
  billed, maximum effective bandwidth by ensuring only highly-
  compressed data traverses the internet.
- **Target platforms:** macOS 15+ (FSKit-based mount is fine) and
  modern Linux. Windows deferred.
- **Every phase must be measurable against JuiceFS** as the honest
  baseline for the S3-backed compressed storage use case.

---

## Phase A — Linux platform port

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
1. Baseline the current binary on the target Linux machine — probably
   already fine on plain NVMe. Establish numbers to beat.
2. `posix_fadvise` / `readahead` hints — small `platform.rs` port.
3. FIEMAP-based extent-order reader — new source implementation,
   similar shape to `RawApfsSource` but no filesystem parser. Works
   across ext4/xfs/btrfs. **~1-2 weeks.**
4. `io_uring` reader — optional per-source. **~1 week + testing.**

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

## Recommended ordering

**Primary constraint (per project owner):**
- **S3 layer is the most critical.** Goal: minimum S3 storage
  billed, maximum effective bandwidth (only highly-compressed data
  crosses the internet).
- **Target platform:** macOS 15+ (FSKit-based mount is acceptable)
  and modern Linux.

Given those constraints, ordering shifts. The S3-critical path is
**C → D → E** with F (JuiceFS) as the honest baseline to beat.

1. **Phase C (`.tpk` chunked container)** — prerequisite for D.
   Nail chunk-index + dedup + on-disk format locally first, so the
   S3 layer becomes purely a backend swap. **~1-2 weeks.**
2. **Phase D (S3 backend for `.tpk`)** — the top priority. Restic-
   style pack layout, content-addressed dedup, zstd (or user-selected
   codec) at rest, Range-GET partial reads. This alone hits the
   "minimum S3 storage + only-compressed-data-transits" target,
   even before any mount exists — because tzip's CLI already knows
   how to pull individual files. **~2-4 weeks.**
3. **Phase E (mount)** — makes the S3-backed store shell-transparent.
   FSKit on macOS 15+, FUSE3 on Linux. Read-only mount first
   (~2 weeks/platform); R/W adds months of edge-case work.
4. **Phase B (`tzip compact`)** — orthogonal quick win for local
   macOS transparent compression. Can slot in any time; doesn't
   block the S3 path. **~3-5 days.**
5. **Phase A (Linux platform port)** — needed if the machines that
   push to S3 include Linux boxes. Otherwise defer.

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
