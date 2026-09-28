# `--raw-block` — raw APFS block reader

**Status:** production; auto-enabled when eligible.
**Platform:** macOS only.
**Cargo feature:** `raw-apfs`.
**Build:** `cargo build --release --features raw-apfs`.

## What it does

Bypasses the VFS layer entirely by opening `/dev/rdiskN` and parsing
the APFS on-disk format directly (via the vendored
[`apfs`](../vendor/apfs) crate, MIT). File `open()` never happens as
far as the kernel's file-descriptor table is concerned — so on-access
AV hooks (Sophos / CrowdStrike / SentinelOne / etc.) that intercept
`open()` via Endpoint Security see nothing.

It's also a straight-up performance win on this drive class: **~2×
faster than the default VFS path on large-archive-style workloads**, and
larger multiples on smaller ones.

## Auto-enable

tzip auto-enables `--raw-block` when all of these hold:

1. Built with the `raw-apfs` Cargo feature.
2. Source is an APFS-on-USB volume (same detector `keep_cache` /
   `dispatch_io` auto-tune already uses).
3. Process is running as root (`sudo tzip …`) — needed to open
   `/dev/rdiskN`, which is `root:operator 0640`.
4. The user did not explicitly pass `--raw-block=false`.

You'll see this in the startup line:

```
tzip: auto-tuned defaults for APFS-on-USB source (
    keep_cache=on, raw_block=on (root+APFS-on-USB), read_jobs=8).
```

If any prerequisite is missing, tzip runs the default VFS reader path
transparently.

## Performance (measured on TestDrive USB SSD, M1 Max, macOS 26.6.2)

| Workload | Default VFS | `--raw-block` | Speedup |
|---|---|---|---|
| reference-archive (79 K files, 11.7 GB → 3.3 GB deflate -x 9 + AES-256) | 2:52 | 1:26 | 2.0× |
| One exam, aged data (11 K files, 1.7 GB, `-m store`) | 2:15 | 0:22 | 6.1× |
| One exam, **fresh copy** (same data, freshly `cp -R`'d) | 2:22 | 0:17 | **8.2×** |

Fresh vs. aged: on a freshly-populated drive, files created together
share adjacent disk offsets, so the bulk reader's extent-order sort
collapses many extents into one big pread. On an aged drive (files
added over months), files are scattered and the sort barely coalesces
— but the whole-tree metadata prefetch alone still delivers ~6×.

Byte-verified: extracted trees are recursively identical between the
two paths (see [`scripts/verify-raw-block.sh`](../scripts/verify-raw-block.sh)).
Reproduce these numbers with [`scripts/bench-raw-block.sh`](../scripts/bench-raw-block.sh).

## Architecture

Four pieces, each attacking a different bottleneck:

### 1. Whole-tree metadata prefetch (`scan_all_metadata`)

The catalog B-tree on a typical APFS volume is small (tens to a few
hundred MB). A single sequential-ish walk of the whole tree reads
every leaf once, extracting every `INODE` and `FILE_EXTENT` record
into two HashMaps: `oid → InodeVal` and `private_id → [(logical_addr,
extent)]`. On TestDrive: 325 K inodes + 320 K extent-lists in ~14 s.

After this, every per-file B-tree lookup becomes a hashmap hit. The
alternative — N per-file `lookup_inode` + `lookup_extents` descents —
is O(N × btree_depth × block_read_latency), and on scattered datasets
(files created over months, so their OIDs are scattered across the
tree) each descent evicts unrelated hot blocks from the block cache.
For N > ~1 K, the one-shot scan wins even on a fully-populated volume.

### 2. Sharded block cache

`/dev/rdiskN` is a character device — it bypasses the OS buffer cache
by design. Without our own cache, every reader thread's B-tree
traversal re-reads interior nodes from the physical device.

The `SharedBlockCache` is 16 shards of `parking_lot::Mutex<HashMap<u64,
Vec<u8>>>`, keyed by block-aligned offset. Shard chosen by `(offset >>
12) % 16` — since offsets are always 4 KiB-aligned, the low 12 bits
carry no shard entropy so this is well-distributed. Result: 16 reader
threads on distinct offsets don't contend on a single lock, and hot
interior nodes are cached once instead of once-per-reader.

FIFO eviction per shard, capped so total footprint stays modest
(currently 64 MiB total).

### 3. Bulk reader (disk-order sequential preads)

Once metadata is cached, the reader threads know every extent's disk
offset without any B-tree work. They:

1. Sort every window's extents by disk offset.
2. Coalesce adjacent extents (gap ≤ 256 KiB) into runs, capped at 64
   MiB per run.
3. `pread` each run in one syscall.
4. Slice the pread'd bytes back to per-file buffers.
5. Ship `RawItem`s to compressors in file-index order (writer sorts on
   the way out, so out-of-order completion is fine).

Wider coalesce gaps (tried 32 MiB) tempt but lose on scattered
datasets: gap bytes multiply total reads 3-5× without adding useful
data.

### 4. Parallel bulk reader threads with dir locality

Reader threads split the item list into N contiguous walker-order
chunks and each runs a bulk reader on their chunk. Walker-order means
same-directory items stay on the same thread, so the shared block
cache stays warm for each thread's working set.

An earlier attempt at parallel bulk failed because per-file
`lookup_inode` was still happening in each thread and thrashed the
cache. Once metadata was globally prefetched, the parallelism became
clean device-I/O parallelism — 8 threads land at 622 % CPU utilization
on this drive.

### Why sequential-order alone didn't win

On a freshly-populated drive where generic-files in one MRI series were
written back-to-back, extents cluster on disk and sorting collapses
tens of files into a single big pread. On TestDrive (files added over
months), files are scattered — sorting by disk offset produces `1
extent/run` on average, no coalescing. The metadata prefetch is what
carries the win here; sequential-order is a small marginal
improvement.

## Access requirements

`/dev/rdisk*` nodes are `root:operator 0640` — regular users can't
read them. Options:

| Approach | Effort | Blast radius |
|---|---|---|
| `sudo tzip …` per invocation | zero setup | minimal — one command |
| `sudo dseditgroup -o edit -a $USER -t user operator` | one-time + logout/login | persistent — any future shell you run |
| setuid tzip binary | not recommended | anyone on the box gets raw-disk access |

## What volumes are supported

**Works:**
- Unencrypted external APFS drives (typical tzip target)
- Unencrypted internal APFS volumes (rare on Macs shipped with
  FileVault by default, but possible)
- Sealed system volumes (readable; not usually what you want to
  archive)

**Does not work: FileVault-encrypted volumes.** Detects as a checksum
error on the first B-tree read; auto-enable falls back cleanly to the
default VFS path.

### FileVault details

On a FileVault-enabled APFS volume the on-disk layout is:

- Container superblock (NX): **unencrypted**
- Volume superblock (APSB): **unencrypted**
- Catalog B-tree pages: **FileVault-encrypted**
- Inode records: **FileVault-encrypted**
- File extent data: **FileVault-encrypted**

Decryption happens in the APFS software driver in the kernel, above
the block layer. When you mount the volume, macOS unlocks the Volume
Encryption Key (VEK) via the SEP and hands it to the APFS driver.
Reads *through* the mounted filesystem get plaintext. Reads directly
from `/dev/rdiskN` do **not** — they return on-disk ciphertext.

Container + volume superblocks parse cleanly (they're unencrypted);
the first catalog B-tree walk hits FileVault-encrypted pages and
Fletcher-64 rejects them as corrupt. Implementing user-space
decryption would require enumerating class keys from the keybag,
retrieving the VEK via `authd`/`SecKeychain`, implementing AES-XTS,
and covering class-key hierarchies — weeks of work + security audit,
for a niche that already has a supported alternative (mount the
volume, use the default VFS path).

## Live volume caveats

On an actively-being-written volume, the "latest checkpoint" is a
moving target. When we resolve an OMAP pointer downstream, the
referenced page might have been superseded.

For tzip's typical use case (bulk archiving from an external drive
that isn't being written to), the latest checkpoint is stable and
this isn't an issue.

For actively-mounted volumes, the correct fix is APFS snapshots
(`sudo tmutil localsnapshot` or `fs_snapshot_create`) plus extending
the `apfs` crate to open at a specific snapshot's XID. Not
implemented.

## AV / EDR alerts on raw disk access

Opening `/dev/rdiskN` is visible to Endpoint Security. Sophos,
CrowdStrike, SentinelOne etc. can subscribe to
`ES_EVENT_TYPE_NOTIFY_OPEN` and log/alert on unusual processes
touching raw block devices. `tzip --raw-block` bypasses the
*scanning* hook (`ES_EVENT_TYPE_AUTH_OPEN`) but cannot hide from the
*notification* hook.

Expected consequence in enterprise environments: your IT admin gets a
Sophos Central event to review. It won't block the operation, but it
may prompt a conversation. If you're using `--raw-block` regularly, a
better long-term arrangement is a Sophos *process-based* exclusion
for the `tzip` binary — that skips scanning for files opened by tzip
without needing raw block access, and doesn't generate the
raw-disk-access alert.

## Testing & verification

### Smoke test — verify the drive is readable

```
cargo build --example raw_apfs_probe --features raw-apfs --release
sudo target/release/examples/raw_apfs_probe /dev/rdiskN
```

Expected on an unencrypted external APFS drive:

```
open: OK
APFS parse: OK
=== volume ===
  name: <your drive name>
  ...
=== / (top 10 of N entries) ===
  Directory  .Spotlight-V100  0 bytes
  ...
```

If you see `list_directory failed: invalid checksum`, the volume is
FileVault-encrypted and `--raw-block` will not work on it.

### Correctness verification — vs default path

```
sudo scripts/verify-raw-block.sh
```

Builds both zips (`-m store`, no password) and byte-diffs the
extracted trees. Exits non-zero if they differ.

### Profiling

For `sample` / Instruments with symbols:

```
cargo build --profile profiling --features raw-apfs
sudo scripts/profile-sample.sh          # 60 s `sample` output
sudo scripts/profile-raw-block.sh       # Instruments trace
```
