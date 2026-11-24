# NAS: 2 million small files on an inode-starved share

## Scenario

A lab has a 2 TB Synology / QNAP / TrueNAS share mounted over SMB or NFS:

- **2 million files**, median ~8 KB (think: sensor readings, experiment
  outputs, millions of 3–15 KB JSONs, or legacy per-image sidecars)
- Total data on disk: ~16 GB logical, ~24 GB after FS block overhead
- Filesystem was formatted with defaults years ago: `mkfs.ext4 -i 16384`
  (one inode per 16 KB of filesystem). That inode ratio was fine for a
  workload averaging 64 KB files. For 8 KB files the inode table is
  undersized — **the share runs out of inodes before it runs out of
  blocks.**

The team hits "No space left on device" at 72% full by `df -h`. `df -i`
shows 99% inode usage. Freeing data space by deleting files fixes
nothing because each deletion returns an inode too.

Backup, scan, and migration are all painful because every file
access over SMB involves per-file metadata round trips.

## Where the pain is

### Metadata overhead dominates

SMB per-file open on a mid-range NAS over gigabit Ethernet:

| Op | Typical latency |
|---|---|
| `stat` (one file) | 0.5–1 ms |
| `open` + `read` + `close` of an 8 KB file | 3–5 ms |
| `readdir` of a 1k-entry directory | ~50–120 ms |

Reading **all 2 million files** sequentially via `rsync` or `cp -r`:

- 2M files × 4 ms per open = **~133 minutes** just for the opens
- Data transfer: 16 GB at ~100 MB/s LAN = ~160 seconds
- Total: ~**2.3 hours**, dominated by metadata

Parallel read via rsync's `--whole-file --inplace -R` doesn't help much:
SMB connection pools are usually small and the NAS's CPU bottlenecks
its own metadata subsystem.

### Inode exhaustion

With `df -i` at 99%:

- `touch newfile` returns `No space left on device` despite 28% free blocks
- Users respond by deleting old files, which frees inodes 1:1 — so the
  problem keeps recurring
- Fix via `mkfs.ext4 -N <new_count>` or `tune2fs -i`? **Not possible on
  an existing ext4 — the inode table is fixed at creation.** The only
  real fix is reformat and restore from backup.
- Reformat means: a long outage (24+ hours for 2 TB over gigabit to a
  staging server), plus someone has to be awake to orchestrate it.

### Backup times out

Nightly `rsync -av /mnt/nas /backup/` takes 3+ hours. If the backup
window is 2 hours it finishes incomplete, and the FS keeps growing.

Cloud-sync tools (rclone to S3, Google Drive sync) are worse — 2M PUTs
means an HTTPS handshake per file. Even with 100 concurrent uploads,
it's 20+ minutes of pure API overhead on top of the per-file metadata
reads from the NAS.

### Small-file disk waste

ext4 at default block size 4 KB and no `-O inline_data`:

- An 8 KB file consumes 2 × 4 KB = exactly 8 KB block + 128 bytes inode = 8.13 KB
- A 1 KB file consumes 4 KB block + inode = **4× overhead**
- Compare the aggregate: 2M × avg 1.5× overhead = extra 8 GB of block waste

## The tzip take

Bundle periodic snapshots of the share into tzip archives. The data lives
once as individual files for live reads, but backup / analysis /
migration operates against a bundled archive.

```
# One-time: build a snapshot. tzip's parallel walker hits every file
# once, streaming into one compressed output.
tzip create nas-snapshot-$(date +%F).zip /mnt/nas/ \
  -m deflate -x 6 \
  --write-sha256

# Copy that one file to S3 / another NAS / tape. ONE transfer, not 2M.
aws s3 cp nas-snapshot-$(date +%F).zip s3://lab-backup/

# Weekly diff vs last week — find what changed without touching the NAS.
tzip diff \
  s3://lab-backup/nas-snapshot-2026-11-17.zip \
  s3://lab-backup/nas-snapshot-2026-11-24.zip

# Migrate to a reformatted NAS: ONE copy, then extract.
tzip x nas-snapshot-2026-11-24.zip --to /mnt/new-nas/ -P 32
```

### Time comparison

| Operation | Native (SMB round-trips) | tzip bundle |
|---|---|---|
| Nightly backup read | ~2.3 hours (metadata-bound) | ~**3 min** (bandwidth-bound) |
| Copy snapshot to S3 | ~20 min HTTPS overhead + ~3 min data | ~**3 min** (single PUT) |
| Compare two snapshots | Not practical (two full walks) | `tzip diff` — body-free, ~**1-5 sec** |
| Restore to a new NAS | ~2.3 hours | ~**3 min** |
| Find all JSON entries larger than 100 KB | `find ... -size +100k ... | wc -l` — full walk | `tzip find --include '*.json' --min-size 100k` — **seconds**, CDR-only |

### Inode pressure: tzip sidesteps it

The on-disk snapshot archive is **one inode**. Incremental backups,
cross-FS copies, and S3 uploads no longer touch the exhausted inode
pool at all.

If the underlying share HAS inode pressure for live writes (not backup),
tzip still helps indirectly:

1. Bundle historical / archive directories that users don't actively
   touch into a `tzip` archive, delete the original files. Each 100k
   files bundled frees 100k inodes **for live writes**.
2. The archive can be mounted read-only via `tzip mount` (FUSE3 on
   Linux, FSKit on macOS 15.4+) so apps still see the files as if
   they were on-disk, but the FS only sees one inode.

```
# Reclaim 500k inodes by archiving the 2023 archive directory.
tzip create /mnt/nas/_archive/2023.zip /mnt/nas/2023/ -m deflate -x 9
rm -rf /mnt/nas/2023

# Apps still see the files via the mount.
mkdir /mnt/2023
tzip mount /mnt/nas/_archive/2023.zip /mnt/2023
```

### Metadata reads become a single tail fetch

`tzip ls nas-snapshot.zip` reads only the archive's Central Directory
Record at the tail of the file — a few megabytes regardless of entry
count. 2 million entries' worth of names + sizes + mtimes come back
in a single CDR read, versus the 2M metadata round trips that a
`find /mnt/nas -printf ...` would need.

### Integrity without metadata churn

Verifying that a backup is consistent usually means `rsync --checksum`
or `find | md5sum` — both require reading every byte of every file.
On this NAS that's 2M opens + 24 GB data = hours.

tzip's `--write-sha256` embeds per-entry SHA-256 in a tzip-native extra
field at create time. Later integrity checks become:

```
tzip hash --algo sha256 /mnt/nas/_archive/nas-snapshot.zip
```

which reads **only the CDR** when hashes are stored (tens of ms for
2M entries), emitting every file's digest. Compare against a stored
baseline: `diff <(tzip hash --algo sha256 old.zip) \
  <(tzip hash --algo sha256 new.zip)`.

## Features at work

- `src/walker.rs` + `src/bulk_walker.rs` — parallel directory walkers
  that batch metadata reads. `bulk_walker` uses macOS `getattrlistbulk`
  to pull thousands of entries per syscall; `walker.rs` uses `jwalk`
  with a rayon pool on Linux.
- `src/pipeline.rs` — multi-stage pipeline: parallel walk → parallel
  reader → parallel compressor → serial writer. Keeps the NAS pipe
  full while compressors work.
- `src/reader.rs::DirFdCache` + `src/dirfd_cache.rs` (PR #46) — extract
  side uses `openat(parent_dirfd, basename)` so restores don't pay
  the NAS's path-walk tax per file.
- `src/sources/http.rs` + `s3.rs` — the same archive, once bundled,
  streams over Range GETs from S3 / HTTP without re-downloading everything.
- `src/mount/` — read-only FUSE3 (Linux) / FSKit (macOS 15.4+) mount of
  an archive so apps see the files unchanged.
- `src/hash.rs` — per-entry crc32 / md5 / sha256, with CDR-only fast
  path when hashes were written at create time (PR #42, #43, #45).

## When this DOESN'T fit

- **Live mutation on the archived files.** The mount is read-only in
  the current release. Files under heavy write traffic stay loose.
  (Phase H3 writable mount support exists but is targeted at a
  different set of use cases.)
- **Hot files accessed on their own.** If one sidecar is read 1000×/s
  on its own path, keep it loose. Bundle only the long-tail/historical
  slice.
- **If the inode exhaustion is on a tiny number of huge files** —
  then you don't have a small-file problem, you have a wrong
  filesystem. tzip doesn't help.

## Rough rule

> A NAS carrying **millions of small files** that are read in bulk
> (backup, analysis, migration) but rarely written one-at-a-time is
> a perfect fit for bundling into a tzip archive. One file replaces
> millions of inodes and millions of SMB round-trips. Live reads can
> still happen via `tzip mount`.
