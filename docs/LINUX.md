# Linux fast paths — FIEMAP extent-order reader + io_uring

This is the Linux counterpart to [`RAW_BLOCK.md`](RAW_BLOCK.md). It documents
the two Linux I/O fast paths, why they exist, how they're wired, and how to
test them.

## TL;DR

- **`--raw-block`** (module `src/linux_raw.rs`, ioctl wrapper `src/fiemap.rs`):
  sort files into physical-disk order via the `FS_IOC_FIEMAP` ioctl and read
  them in that order, turning a seek storm into a forward sweep. No feature
  flag; always compiled in on Linux. Works on ext4/xfs/btrfs/f2fs.
- **`--io-uring`** (module `src/io_uring_src.rs`, Cargo feature `io-uring`):
  read file bodies via io_uring at queue depth 128 so a single thread can
  saturate NVMe. Composes with `--raw-block`.
- **`--apfs-device`** (module `src/raw_apfs.rs`, Cargo feature `raw-apfs`):
  archive an APFS volume by parsing it directly off a raw device — **no mount,
  no APFS driver**. See "Reading APFS on Linux" below.

## Reading APFS on Linux (`--apfs-device`)

`tzip --apfs-device /dev/sda1 out.zip` archives an APFS volume that Linux can't
even mount, by running the same on-disk parser that powers the macOS
`--raw-block` path (vendored `apfs` crate, `vendor/apfs`). The parser is
platform-independent — it operates on a `Read + Seek` over the device — so the
only Linux-specific concern is the device node itself (a *block* device
`/dev/sdXN` instead of macOS's *char* device `/dev/rdiskN`; the block-aligned
reader works on both).

**Why not FIEMAP for APFS?** FIEMAP is an ioctl serviced by the *mounted*
filesystem's kernel driver. An unmounted APFS device has no driver answering
it, and apfs-fuse (read-only) doesn't implement `fiemap`. So FIEMAP can't help
here — the raw parser is both the only option and the faster one (a single
sequential catalog scan up front, then O(1) hashmap lookups per file).

**Design.** `RawApfsSource::open_for_device(dev)` opens the device directly and
sets `mount_point = /`, so CLI roots are treated as absolute *in-volume* paths
(`/Users/me/...`); with no roots, the whole volume is walked from `/`. Walk and
read both come from the parser (`walk_stream` → `scan_all_metadata` →
`read_file_by_oid`). All reader backends (raw-APFS, FIEMAP, io_uring) share one
bulk path via `Source::read_bulk`.

**Access.** The node is `root:disk 0660`. Use `sudo`, join the `disk` group
(`sudo usermod -aG disk $USER`, persistent across reboots), or a temporary ACL
(`sudo setfacl -m u:$USER:r /dev/sda1` — udev may reset it when the node is
re-created). A failed open is fatal: there's no VFS fallback for an unmounted
device.

**Not supported.** FileVault / block-layer-encrypted volumes (catalog pages are
ciphertext at the block layer) — detected up front with a clear message.

**Verification.** Since the volume isn't mounted there's no ground-truth mount
to byte-compare against, so correctness is checked by extracting and validating
file-format integrity of structured files (a parser offset/extent bug corrupts
them): e.g. `unzip -t` for CRC, `%PDF`/`%%EOF` + `pdfinfo` for PDFs, and the
`DICM` magic at byte 128 for DICOM. Verified against a 931 GB Mac-formatted USB
disk: whole-catalog walk (451k inodes / 445k extent-lists in 2.7 s), valid
multi-MB PDF, 72/72 DICOM files valid, clean CRC.

## Why FIEMAP instead of a raw-device parser

On macOS the `--raw-block` reader parses the APFS on-disk format straight off
`/dev/rdiskN`, which serves two goals at once: sequential disk-order reads
*and* bypassing the VFS (and the on-access AV hooks layered on it).

On Linux the two goals split apart:

- **Disk-order reads** don't need a parser. The kernel already exposes each
  file's physical block layout through the standard `FS_IOC_FIEMAP` ioctl,
  implemented by every major filesystem (ext4, xfs, btrfs, f2fs). We ask for
  each file's first physical extent, sort by it, and read through the normal
  VFS in that order. No vendored filesystem code, works everywhere FIEMAP
  does.
- **AV bypass** is a *different* problem on Linux. Enterprise AV (Sophos,
  CrowdStrike Falcon) hooks via `fanotify` + `LD_PRELOAD` shims, not raw
  device access. Reading in physical order through the VFS doesn't dodge them.
  The honest answer stays "get a per-process exclusion." So the Linux
  `--raw-block` is a *throughput* optimization, not an AV-evasion one.

## How the extent-order reader works

`LinuxExtentSource::bulk_read_all` (in `src/linux_raw.rs`) processes the feed
in windows of `WINDOW = 256` files:

1. **Open + probe.** For each file, `openat(dirfd, basename)` (falling back to
   open-by-path), then `fiemap::first_physical_offset(fd)` — one ioctl asking
   for a single extent. Files whose layout is unavailable (unsupported fs,
   sparse/inline/delalloc extents, open error) get a sentinel offset of
   `u64::MAX` so they sort to the end.
2. **Sort.** The window is stably sorted by physical offset, so same-offset
   files keep their walk order.
3. **Read.** Files are read in disk order. `posix_fadvise`/`readahead` is
   issued for the next `PREFETCH_DEPTH = 4` files (that are at least
   `PREFETCH_MIN_BYTES = 256 KiB`) so their blocks are in flight by the time
   we reach them.

The window's `index` is carried through untouched, so the writer's `--sort`
reordering and the default arrival-order write both keep working — only the
*read* order changes, never the archive order.

Reader concurrency follows `--read-jobs`: the feed is split into that many
contiguous walker-order chunks, each sorted independently. `--read-jobs 1`
gives a single global forward sweep (best for one spinning disk);
`--read-jobs N` lets several sweeps overlap (helps NVMe / multi-queue).

### The ioctl ABI

`FS_IOC_FIEMAP = _IOWR('f', 11, struct fiemap)`. `src/fiemap.rs` computes the
ioctl number with the asm-generic `_IOC` bit layout, which is correct on x86,
x86_64, arm, aarch64, riscv and s390. On the oddball architectures (mips,
powerpc, sparc, alpha) the layout differs; there FIEMAP simply doesn't fire
and the reader falls back to walk order — no misbehavior, just no speedup.

## How the io_uring reader works

`io_uring_src::read_batch` (feature `io-uring`) takes a batch of `(fd, len)`
and reads each file fully into an owned buffer, keeping up to `QUEUE_DEPTH =
128` reads outstanding. Completions can land out of submission order;
`emit(job_index, buffer)` fires per file and the pipeline reorders by the
carried index if `--sort` is set. Short reads are re-submitted for the
remainder; zero-length files are emitted immediately.

When both flags are on, `bulk_read_all` still does the FIEMAP probe + sort
first, then hands the disk-ordered window to `read_batch` — so submissions go
out in physical order while enjoying deep queueing.

## Auto-tune

`auto_tune` (in `src/pipeline.rs`) reads the block device backing the first
source path via `/sys/dev/block/<maj>:<min>` (`linux_raw::device_traits`):

- **rotational == 1** or **removable == 1**, and FIEMAP is supported →
  `raw_block=on`, and `read_jobs=1` unless the user set it. A seeking device
  wants one clean sweep.
- **NVMe / SSD** → left off. The reorder is neutral there, and we'd rather not
  pay the probe cost or surprise anyone. Pass `--raw-block` to force it.

`--io-uring` is never auto-enabled; it's opt-in.

## Testing

Correctness is verified the same way as the macOS path: the extent-order and
io_uring readers must produce **byte-identical** archives to the default
reader.

```
# byte-identical with --sort (reproducible order)
tzip a.zip src --sort -q
tzip b.zip src --sort -q --raw-block
tzip c.zip src --sort -q --raw-block --io-uring   # needs --features io-uring
cmp a.zip b.zip && cmp a.zip c.zip && echo "identical"

# round-trip
mkdir out && (cd out && unzip -q ../b.zip) && diff -r src out/src

# confirm FIEMAP is actually resolving offsets
tzip /dev/null.zip src --raw-block -v 2>&1 | grep extent-order
# -> "extent-order reader probed N files, N had a usable physical offset"
```

### Measuring the real win

The speedup only shows on **cold cache** on **seeking media**. On warm cache
or NVMe the FIEMAP probe + sort is pure overhead (a few tenths of a second on
thousands of files) with no seeks to recover, so it looks neutral-to-slightly
negative — which is why auto-tune keeps it off there.

To measure honestly you need to drop the page cache between runs (requires
root) and a rotational or USB drive:

```
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
tzip out.zip /mnt/usb-hdd/src --raw-block=false -q   # baseline
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
tzip out.zip /mnt/usb-hdd/src --raw-block -q         # extent-order
```

## Not covered / future work

- **Cross-file extent coalescing.** We read each file through its own fd, so
  we can't merge a single `pread` across adjacent files the way the macOS
  raw-device reader does. Doing that would require `O_DIRECT` reads against the
  block device (and re-slicing buffers back to files) — a larger project.
- **`getdents64` + `statx` bulk walker.** The walk still uses jwalk. A
  Linux-native bulk walker is the analog of the macOS `getattrlistbulk`
  walker; not yet built.
