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
- [TUI mode](#tui-mode)
- [Performance](#performance)
- [External-drive tuning](#external-drive-tuning)
- [macOS-specific fast paths](#macos-specific-fast-paths)
- [APFS-on-USB warning](#apfs-on-usb-warning)
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

### System requirements at runtime

- macOS 11+ (universal support for ARMv8 crypto + libdispatch + `getattrlistbulk`)
- Linux with kernel 4.14+ (SHA-NI/AES-NI at runtime via `cpufeatures`)
- Windows 10+ (build with `cargo build --release` from a POSIX-emulated shell
  such as MSYS2 — the bulk_walker and dispatch_io modules are compiled out
  automatically)

---

## Quick start

```
# Create a plain .zip archive
tzip out.zip path/to/source_tree

# With max DEFLATE and AES-256
tzip out.zip src/ -m deflate -x 12 -p 'hunter2'

# Read the password from a file
tzip out.zip src/ --password-file ~/.archive.pw

# Zstd for high ratio + speed (recipient needs a zstd-aware unzipper)
tzip out.zip src/ -m zstd -x 19

# .7z solid LZMA2 with header encryption
tzip out.7z src/ --solid -p 'hunter2'

# Bigger files → auto-triggers parallel-block DEFLATE
tzip out.zip huge_log_file.txt

# Deterministic byte-identical builds
tzip out.zip src/ --sort
```

---

## CLI reference

```
tzip [OPTIONS] <ARCHIVE> <PATHS>...
```

**Positional**

| arg | meaning |
|---|---|
| `<ARCHIVE>` | Output archive path. `.7z` extension auto-enables `--solid`. |
| `<PATHS>...` | One or more files or directories to archive. |

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

**Output selection & ordering**

| flag | default | meaning |
|---|---|---|
| `--sort` | off | Deterministic order — same inputs produce byte-identical archives (STORE mode; AES randomizes salt so byte-identical AES output requires additional work) |
| `--exclude <GLOB>` | none | Repeatable simple glob (`*` `?`) matched against the archive path |

**Progress**

| flag | default | meaning |
|---|---|---|
| `-q, --quiet` | off | No progress bar |
| `-v, --verbose` | off | Print each added file to stderr |
| `--tui` | off | Ratatui dashboard (per-worker MB/s, overall gauge, `q`/Esc to cancel) when stdout is a TTY |

Full help: `tzip --help`.

---

## Compression methods

All methods compose orthogonally with AES-256.

| method | ZIP ID | crate | notes |
|---|---|---|---|
| `store` | 0 | — | Uncompressed. Fast; use for pre-compressed data (JPEGs, MP4, `.gz`) |
| `deflate` | 8 | `libdeflater` | Default. 30-40% faster than flate2. Levels 0-12; 12 ≈ zopfli-lite |
| `bzip2` | 12 | `bzip2` | 5-10% better than DEFLATE on text; ~4× slower |
| `lzma` | 14 | `xz2` (LZMA1) | Better ratio than DEFLATE; ZIP consumers need LZMA support |
| `xz` | 95 | `xz2` (LZMA2) | Same LZMA2 as `.7z`, but per-file inside a ZIP |
| `zstd` | 93 | `zstd` | Great ratio + speed. Recipient needs zstd-aware unzipper |

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

---

## `.7z` solid archives

`--solid` (or archive extension `.7z`) switches to `sevenz-rust` and emits an
LZMA2 solid stream:

- Substantially better ratio on similar-file corpora (DICOM, logs, source
  trees, JSON dumps) — the dictionary spans all files
- With a password, header encryption is enabled (filenames are hidden)
- **Single-threaded**: sevenz-rust 0.6 does not expose multi-threaded LZMA2
  (`-mmt=on` in p7zip). `--jobs` is ignored for `.7z` output.

Workarounds if speed matters more than ratio on `.7z`:
- Use `-m xz` inside a `.zip` (still per-file parallel across files)
- Shell out to system `p7zip` with `7z a -mmt=on -m0=lzma2`

---

## TUI mode

`--tui` when stdout is a TTY enables a ratatui alternate-screen dashboard:

```
┌ tzip ─────────────────────────────────────────────────────────────────┐
│ 2.3 GB / 9.1 GB  •  38k / 143k files  •  412 MB/s  •  ETA 0m17s       │
│ [===============================>............................] 25%   │
└───────────────────────────────────────────────────────────────────────┘
┌ workers ──────────────────────────────────────────────────────────────┐
│ worker  0: ▶ ...ry/26690/MRIUnenhance/2103_ax_DWI.../IM_82.dcm  511 MB/s
│ worker  1: ▶ ...ry/26685/MRIUnenhance/1804_localizer.../IM_3.dcm  489 MB/s
│ worker  2: · idle                                                 0 MB/s
│ ...
└───────────────────────────────────────────────────────────────────────┘
```

Keys: `q` or Esc to signal cancel. Cancel is currently cooperative — the
pipeline finishes in-flight items before exiting.

---

## Performance

### Design summary

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

**Deep DICOM trees (many small files, deep dirs):**
```
tzip out.zip /path/to/studies/ --read-jobs auto --walk-jobs 8
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
On DICOM trees (5-6 levels deep) this saves 5-6 metadata lookups per
file open.

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

---

## APFS-on-USB warning

**tl;dr: do not use APFS on USB-attached drives if you care about
throughput. Use exfat with 32 KB allocation unit instead.**

APFS's B-tree metadata and extent-based storage are ideal on internal
NVMe (µs-latency PCIe). Over USB-MSC (USB Mass Storage Class), every
metadata read becomes a synchronous USB command with ~1-3 ms overhead.
APFS's small-random-metadata pattern hits USB-MSC's worst-case latency
profile.

Symptoms:
- `dd if=<any-large-file> of=/dev/null bs=1m count=200` on an APFS-USB
  volume runs at 3-10 MB/s where exfat on the same hardware runs at
  50-150 MB/s
- `readdir` of a directory with a few thousand files takes 30+ seconds
- Any archiver (tzip, p7zip, ditto, WinZip) appears "hung"; the
  archiver isn't the bottleneck, the driver stack is

If you're stuck with APFS on USB and can't reformat, the tzip
mitigations that help most:

```
tzip out.zip src/ --keep-cache --read-jobs 1 --dispatch-io
```

- `--keep-cache` — let readahead work; F_NOCACHE was the wrong default
- `--read-jobs 1` — one reader stops fighting itself for the USB head
- `--dispatch-io` — GCD may pipeline requests better than blocking threads

None of these recover exfat parity; they take the edge off.

### If you can reformat

In Disk Utility, select the *physical disk* (not the volume) → Erase →
Format `ExFAT` → Allocation Unit Size `32 KB` (or via terminal:
`newfs_exfat -c 32k`). GUID Partition Map. Then rsync your data back.

- 32 KB clusters eliminate the "1 MiB slack per file" waste that
  motivates APFS in the first place
- exfat over USB-MSC is fast because its access pattern is what USB was
  designed for

---

## Known limitations

- **`.7z` solid is single-threaded** — sevenz-rust 0.6 does not expose
  multi-threaded LZMA2. Use `-m xz` inside a `.zip` for parallelism.
- **Bulk walker sets mtime = DOS epoch** — the `getattrlistbulk`
  attribute-alignment parser doesn't currently decode `ATTR_CMN_MODTIME`
  correctly; the field is skipped and mtime is set to 1980-01-01. Use
  `--classic-walk` if real mtimes matter. TODO.
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

The suite in `tests/roundtrip.rs` covers:

- STORE round-trip via `zip` crate extraction
- DEFLATE at levels 0 and 12
- BZIP2, LZMA, XZ, Zstd round-trip via system `7z`
- AES-256 archive structural validation
- `.7z` solid round-trip (plain + AES)
- `--sort` byte-identical determinism on STORE archives

11 tests total. All pass in ~0.5s.

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
