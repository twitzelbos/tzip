//! Raw APFS block reader — bypasses the VFS + any on-access AV.
//!
//! Enabled with the Cargo feature `raw-apfs`. Uses the vendored (patched)
//! `apfs` parser (see `vendor/apfs`, MIT) to walk an APFS volume by
//! reading `/dev/rdiskN` directly. This avoids the per-file `open()`
//! syscall that Sophos/CrowdStrike/etc. hook via Endpoint Security.
//!
//! See [`docs/RAW_BLOCK.md`](../../docs/RAW_BLOCK.md) for design + testing.
//!
//! ## Concurrency model
//!
//! `ApfsVolume` requires `&mut self` for every operation (it walks the
//! catalog + extent tree via an internal reader state). Rather than
//! serialize the whole reader stage behind a single Mutex, we open **N
//! independent `ApfsVolume` instances** (each with its own file
//! descriptor on `/dev/rdiskN`) and hand them out via a lock-free
//! crossbeam channel: each `read()` pops one instance, uses it, pushes
//! it back. Reader threads block waiting only when the pool is
//! exhausted; sizing the pool to the reader-thread count means normal
//! traffic never waits.
//!
//! Container-superblock parsing takes ~60 ms per instance, so opening
//! e.g. 4 instances costs ~240 ms at startup. Negligible for archive
//! workloads.
//!
//! **Access:** `/dev/rdisk*` is root:operator 0640. Run as `sudo tzip …`
//! or add yourself to the `operator` group.
//!
//! **Not supported:** FileVault-encrypted volumes — the catalog B-tree
//! pages are FileVault-encrypted and decryption happens in the APFS
//! kernel driver, above the block layer. Raw reads return ciphertext
//! → `invalid checksum`.

#![cfg(all(feature = "raw-apfs", target_os = "macos"))]

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use apfs::{ApfsVolume, EntryKind};
use apfs::catalog::InodeVal;

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use crate::pipeline::Source;
use crate::platform::ReadBuf;
use crate::walker::WorkItem;

/// Read+Seek adapter over a macOS character-device file (`/dev/rdiskN`).
///
/// Character devices require reads at block-aligned offset AND block-aligned
/// length. This adapter rounds each `read` call to the block boundary and
/// hands the caller their requested byte range.
///
/// The block cache is **shared** across every `AlignedRawReader` on the
/// same device. `/dev/rdiskN` bypasses the OS buffer cache entirely, so
/// without our own cache every reader thread re-reads catalog B-tree
/// interior nodes from the device on each descent. Sharing the cache
/// means those hot interior nodes are fetched once and served from RAM
/// to every reader.
///
/// The cache stores single-block granularity; the more common large-batch
/// extent reads (4 MiB+) bypass it to keep memory bounded and to avoid
/// evicting the interior-node hot set.
pub struct AlignedRawReader {
    file: File,
    pos: u64,
    block: u64,
    scratch: Vec<u8>,
    cache: SharedBlockCacheRef,
}

/// Shared, thread-safe block cache. Keys are block-aligned device
/// offsets; values are one block of bytes.
///
/// **Sharded** across `SHARDS` independent `parking_lot::Mutex` buckets
/// so N reader threads on different offsets don't contend on a single
/// lock. Shard is picked by `(offset >> 12) % SHARDS` — since offsets
/// are always block-aligned (4 KiB), the low 12 bits carry no shard
/// entropy. FIFO eviction is per-shard, bounding total capacity to
/// `SHARDS × per_shard_capacity` entries.
///
/// Contrast with the earlier single-Mutex design: at 16 reader threads
/// against a 10-core M1 Max the single lock became the hot spot and
/// per-file `read` regressed from 13 ms → 29 ms. This design serializes
/// only readers hitting the same shard.
const SHARDS: usize = 16;

pub struct SharedBlockCache {
    shards: Vec<parking_lot::Mutex<BlockCacheInner>>,
    /// Total block-shift needed for shard picking; cached to avoid
    /// recomputing per get/put.
    block_shift: u32,
}

pub struct BlockCacheInner {
    map: HashMap<u64, std::sync::Arc<Vec<u8>>>,
    /// FIFO of insertion order — evict oldest when capacity is reached.
    order: std::collections::VecDeque<u64>,
    capacity: usize,
    block: usize,
}

impl BlockCacheInner {
    pub fn new(capacity: usize, block: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            block,
        }
    }
    fn get(&self, off: u64) -> Option<std::sync::Arc<Vec<u8>>> {
        self.map.get(&off).cloned()
    }
    fn put(&mut self, off: u64, data: &[u8]) {
        if data.len() != self.block {
            return;
        }
        if self.map.contains_key(&off) {
            return;
        }
        while self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
        self.map
            .insert(off, std::sync::Arc::new(data.to_vec()));
        self.order.push_back(off);
    }
}

impl SharedBlockCache {
    fn shard_for(&self, off: u64) -> &parking_lot::Mutex<BlockCacheInner> {
        let idx = ((off >> self.block_shift) as usize) % self.shards.len();
        &self.shards[idx]
    }
    pub fn get(&self, off: u64) -> Option<std::sync::Arc<Vec<u8>>> {
        self.shard_for(off).lock().get(off)
    }
    pub fn put(&self, off: u64, data: &[u8]) {
        self.shard_for(off).lock().put(off, data)
    }
}

pub type SharedBlockCacheRef = std::sync::Arc<SharedBlockCache>;

pub fn new_shared_block_cache(
    total_capacity_blocks: usize,
    block_size: usize,
) -> SharedBlockCacheRef {
    let per_shard = (total_capacity_blocks / SHARDS).max(1);
    let mut shards = Vec::with_capacity(SHARDS);
    for _ in 0..SHARDS {
        shards.push(parking_lot::Mutex::new(BlockCacheInner::new(
            per_shard, block_size,
        )));
    }
    let block_shift = (block_size as u32).trailing_zeros();
    std::sync::Arc::new(SharedBlockCache {
        shards,
        block_shift,
    })
}

impl AlignedRawReader {
    pub fn new(file: File, block: u64, cache: SharedBlockCacheRef) -> Self {
        Self {
            file,
            pos: 0,
            block,
            scratch: Vec::new(),
            cache,
        }
    }
}

/// Read widening / prefetch window. Small reads (typical: single 4 KiB
/// B-tree node) get expanded to this size so the per-syscall overhead
/// of `/dev/rdiskN` (empirically ~1–5 ms per call regardless of size)
/// is amortized across many blocks. The extra blocks land in the block
/// cache and satisfy subsequent nearby B-tree lookups without another
/// syscall — B-tree leaves and internal pages for one dir are usually
/// laid out contiguously.
const READ_AHEAD: u64 = 256 * 1024;

impl Read for AlignedRawReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let want = out.len();
        if want == 0 {
            return Ok(0);
        }
        let start = self.pos;
        let end = start.saturating_add(want as u64);
        let aligned_start = start & !(self.block - 1);
        let aligned_end = end
            .checked_add(self.block - 1)
            .map(|v| v & !(self.block - 1))
            .unwrap_or(end);
        let req_aligned_len = (aligned_end - aligned_start) as usize;

        // Cache hit path — only applies when the caller's request fits
        // inside one cached block (the common B-tree case). Sharded
        // lookup: no contention unless another thread is racing for
        // the same shard.
        if req_aligned_len == self.block as usize {
            if let Some(cached) = self.cache.get(aligned_start) {
                let off = (start - aligned_start) as usize;
                let n = want.min((self.block as usize) - off);
                out[..n].copy_from_slice(&cached[off..off + n]);
                self.pos += n as u64;
                return Ok(n);
            }
        }

        // Widen the physical read to amortize syscall overhead.
        // For small requests, pull READ_AHEAD bytes and cache the rest.
        // For large requests (extent slurps), read exactly what was
        // asked — big reads don't need widening and shouldn't evict
        // the interior-node hot set.
        let widen = req_aligned_len < (READ_AHEAD as usize);
        let phys_len = if widen {
            READ_AHEAD as usize
        } else {
            req_aligned_len
        };

        if self.scratch.len() < phys_len {
            self.scratch.resize(phys_len, 0);
        }
        self.file.seek(SeekFrom::Start(aligned_start))?;
        let mut got = 0;
        while got < phys_len {
            match self.file.read(&mut self.scratch[got..phys_len]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // Cache every whole block we pulled — but only for the widened
        // (small-request) path. An extent slurp bypasses the cache to
        // preserve the interior-node hot set. Each block goes to its
        // own shard, so per-block put calls parallelize with other
        // readers on different shards.
        if widen {
            let mut off = 0usize;
            let mut abs = aligned_start;
            while off + (self.block as usize) <= got {
                self.cache
                    .put(abs, &self.scratch[off..off + self.block as usize]);
                off += self.block as usize;
                abs += self.block;
            }
        }
        let off = (start - aligned_start) as usize;
        let avail = got.saturating_sub(off);
        let n = want.min(avail);
        out[..n].copy_from_slice(&self.scratch[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for AlignedRawReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(o) => o,
            SeekFrom::Current(d) => (self.pos as i64 + d) as u64,
            SeekFrom::End(_) => {
                let n = self.file.seek(pos)?;
                self.pos = n;
                return Ok(n);
            }
        };
        self.pos = new_pos;
        Ok(new_pos)
    }
}

/// Source implementation backed by raw APFS block reads.
///
/// Owns a pool of `ApfsVolume` instances shared via a bounded crossbeam
/// channel. Each `read()` pops one, uses it, returns it to the pool.
/// Sized to the reader thread count so normal traffic never blocks.
///
/// The catalog `resolve_path` walk from the root costs ~1 s per call on
/// USB drives (many small aligned preads for each B-tree level). To
/// avoid paying that per file, we cache **volume-relative parent-dir
/// OIDs** on first access: the first file in a new parent dir does the
/// expensive walk, and while we're there we `list_directory_by_oid` the
/// whole parent and cache every child OID too. Subsequent files in the
/// same directory are O(1) map lookup + fast `read_file_by_oid`.
pub struct RawApfsSource {
    mount_point: PathBuf,
    device: PathBuf,
    pool_send: Sender<ApfsVolume<AlignedRawReader>>,
    pool_recv: Receiver<ApfsVolume<AlignedRawReader>>,
    /// Volume-relative path (as `/a/b/c/file.ext`) → catalog OID.
    /// Populated lazily: a miss triggers `open_directory` on the parent
    /// and `list_directory_by_oid` of that parent's children.
    oid_cache: RwLock<HashMap<String, u64>>,
    /// Inode data cache keyed by OID. Populated up front by the
    /// one-shot `scan_all_metadata` in `open_for_mount` (fast, single
    /// pass), so the reader hot path is a pure hashmap lookup — no
    /// B-tree work per file.
    inode_cache: RwLock<HashMap<u64, InodeVal>>,
    /// Extent cache keyed by `private_id` (from the inode). Populated
    /// by the same one-shot metadata scan. Reader-side lookup replaces
    /// the per-file `lookup_extents` B-tree descent.
    extent_cache: RwLock<HashMap<u64, Vec<(u64, apfs::catalog::FileExtentVal)>>>,
    /// Block cache shared across every `ApfsVolume` reader on this
    /// source — interior B-tree nodes are read once and served from
    /// RAM to every reader thread. Sharded so N readers on different
    /// offsets don't contend on a single lock.
    block_cache: SharedBlockCacheRef,
    stats: ReadStats,
    /// If true, print the per-phase stats + prefetch line at Drop.
    /// Wired from `Options::verbose` at open time.
    verbose: bool,
}

/// Accumulated per-stage cost of the reader hot path. Dumped at `Drop`.
#[derive(Default)]
pub struct ReadStats {
    // Per-file reader path (`Source::read`).
    files: std::sync::atomic::AtomicU64,
    total_bytes: std::sync::atomic::AtomicU64,
    pool_wait_ns: std::sync::atomic::AtomicU64,
    resolve_ns: std::sync::atomic::AtomicU64,
    read_ns: std::sync::atomic::AtomicU64,
    total_ns: std::sync::atomic::AtomicU64,
    // Bulk (disk-order) reader path.
    bulk_windows: std::sync::atomic::AtomicU64,
    bulk_files: std::sync::atomic::AtomicU64,
    bulk_meta_ns: std::sync::atomic::AtomicU64,
    bulk_scan_ns: std::sync::atomic::AtomicU64,
    bulk_read_ns: std::sync::atomic::AtomicU64,
    bulk_special_ns: std::sync::atomic::AtomicU64,
    bulk_extent_tasks: std::sync::atomic::AtomicU64,
    bulk_coalesced_runs: std::sync::atomic::AtomicU64,
    bulk_run_bytes: std::sync::atomic::AtomicU64,
    bulk_inode_batches: std::sync::atomic::AtomicU64,
    bulk_inode_batches_skipped: std::sync::atomic::AtomicU64,
    bulk_extent_batches: std::sync::atomic::AtomicU64,
    bulk_extent_batches_skipped: std::sync::atomic::AtomicU64,
    bulk_special_files: std::sync::atomic::AtomicU64,
}

impl ReadStats {
    fn record(
        &self,
        bytes: u64,
        pool_wait: std::time::Duration,
        resolve: std::time::Duration,
        read: std::time::Duration,
        total: std::time::Duration,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        self.files.fetch_add(1, Relaxed);
        self.total_bytes.fetch_add(bytes, Relaxed);
        self.pool_wait_ns.fetch_add(pool_wait.as_nanos() as u64, Relaxed);
        self.resolve_ns.fetch_add(resolve.as_nanos() as u64, Relaxed);
        self.read_ns.fetch_add(read.as_nanos() as u64, Relaxed);
        self.total_ns.fetch_add(total.as_nanos() as u64, Relaxed);
    }
}

impl Drop for RawApfsSource {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        // Stats are only interesting for perf debugging — gate on -v so
        // regular runs don't dump a paragraph of numbers.
        if !self.verbose {
            return;
        }
        let n = self.stats.files.load(Relaxed);
        if n > 0 {
            let bytes = self.stats.total_bytes.load(Relaxed);
            let pool_us = self.stats.pool_wait_ns.load(Relaxed) / 1_000;
            let resolve_us = self.stats.resolve_ns.load(Relaxed) / 1_000;
            let read_us = self.stats.read_ns.load(Relaxed) / 1_000;
            let total_us = self.stats.total_ns.load(Relaxed) / 1_000;
            eprintln!(
                "tzip: --raw-block reader stats: {} files, {} MB\n\
                 \tavg per file: pool_wait={}μs resolve={}μs read={}μs total={}μs\n\
                 \tsum across threads: pool_wait={}s resolve={}s read={}s total={}s",
                n,
                bytes / (1024 * 1024),
                pool_us / n,
                resolve_us / n,
                read_us / n,
                total_us / n,
                pool_us / 1_000_000,
                resolve_us / 1_000_000,
                read_us / 1_000_000,
                total_us / 1_000_000,
            );
        }
        let bulk_files = self.stats.bulk_files.load(Relaxed);
        if bulk_files > 0 {
            let windows = self.stats.bulk_windows.load(Relaxed);
            let meta_ms = self.stats.bulk_meta_ns.load(Relaxed) / 1_000_000;
            let scan_ms = self.stats.bulk_scan_ns.load(Relaxed) / 1_000_000;
            let read_ms = self.stats.bulk_read_ns.load(Relaxed) / 1_000_000;
            let spec_ms = self.stats.bulk_special_ns.load(Relaxed) / 1_000_000;
            let tasks = self.stats.bulk_extent_tasks.load(Relaxed);
            let runs = self.stats.bulk_coalesced_runs.load(Relaxed);
            let run_bytes = self.stats.bulk_run_bytes.load(Relaxed);
            let inode_batches = self.stats.bulk_inode_batches.load(Relaxed);
            let inode_batches_skipped = self.stats.bulk_inode_batches_skipped.load(Relaxed);
            let extent_batches = self.stats.bulk_extent_batches.load(Relaxed);
            let extent_batches_skipped = self.stats.bulk_extent_batches_skipped.load(Relaxed);
            let specials = self.stats.bulk_special_files.load(Relaxed);
            let coalesce_ratio = if runs > 0 { tasks / runs } else { 0 };
            let avg_run_bytes = if runs > 0 { run_bytes / runs } else { 0 };
            eprintln!(
                "tzip: --raw-block bulk stats: {} files across {} windows, {} MB read\n\
                 \tphase timings (wall): meta={}ms scan-sort={}ms sequential-read={}ms special-fallback={}ms\n\
                 \tsequential reads: {} extents coalesced into {} runs ({} extents/run avg), avg run {} KB\n\
                 \tbatch metadata: inode batches {} succeeded / {} skipped (range too wide);\n\
                 \t                extent batches {} succeeded / {} skipped\n\
                 \tspecial-case fallbacks (symlink/compressed): {}",
                bulk_files, windows, run_bytes / (1024 * 1024),
                meta_ms, scan_ms, read_ms, spec_ms,
                tasks, runs, coalesce_ratio, avg_run_bytes / 1024,
                inode_batches, inode_batches_skipped,
                extent_batches, extent_batches_skipped,
                specials,
            );
        }
    }
}

impl RawApfsSource {
    /// Open `pool_size` independent readers against the APFS container
    /// backing `mount_point`. Requires root or `operator` group.
    pub fn open_for_mount(
        mount_point: &Path,
        pool_size: usize,
        verbose: bool,
    ) -> Result<Self> {
        let bsd = bsd_device_for_mount(mount_point).with_context(|| {
            format!("resolve /dev/... for mount {}", mount_point.display())
        })?;
        let whole = whole_disk_raw(&bsd)?;

        let (pool_send, pool_recv) = crossbeam_channel::bounded(pool_size.max(1));

        // Shared block cache: 64 MiB total (16 K blocks × 4 KiB). Big
        // enough to hold the catalog + omap B-trees' hot interior nodes
        // for any realistic volume, so every reader thread's B-tree
        // descent hits RAM after the first read.
        let block_cache = new_shared_block_cache(16 * 1024, 4096);

        // One open for real: if this fails, the whole feature is unavailable.
        let first = open_volume(&whole, block_cache.clone())?;
        // FileVault / block-layer encryption check — bail early with a
        // clear message instead of failing deep inside the metadata
        // scan with `invalid checksum`. Catalog pages on encrypted
        // volumes are ciphertext at the block layer; Fletcher-64 would
        // reject every one of them.
        if first.volume_info().encrypted {
            return Err(anyhow!(
                "volume {:?} is block-layer encrypted (FileVault / Apple-Silicon single-key) — \
                 `/dev/rdiskN` reads return ciphertext, so --raw-block cannot parse the catalog. \
                 See docs/RAW_BLOCK.md for details.",
                first.volume_info().name
            ));
        }
        pool_send
            .send(first)
            .map_err(|_| anyhow!("pool send after first open"))?;
        for i in 1..pool_size.max(1) {
            match open_volume(&whole, block_cache.clone()) {
                Ok(v) => {
                    let _ = pool_send.send(v);
                }
                Err(e) => {
                    eprintln!(
                        "tzip: --raw-block pool: only opened {} of {} ({e})",
                        i, pool_size
                    );
                    break;
                }
            }
        }

        // One-shot whole-tree metadata prefetch. The catalog B-tree is
        // small (tens to a few hundred MB on typical volumes) and reads
        // mostly-sequentially. Doing this ONCE up front is dramatically
        // cheaper than N per-file `lookup_inode` + `lookup_extents`
        // descents when N is in the tens of thousands, because each
        // interior node is read once instead of ~N times, and each
        // leaf is read at most once. After this, the reader hot path
        // is a pure hashmap lookup — zero B-tree work per file.
        let (inode_map, extent_map) = {
            // Pop one volume from the pool, scan, put it back.
            let mut vol = pool_recv
                .recv()
                .map_err(|_| anyhow!("pool empty at scan_all_metadata"))?;
            let t = std::time::Instant::now();
            let res = vol.scan_all_metadata();
            let elapsed = t.elapsed();
            let _ = pool_send.send(vol);
            let (imap, emap) = res.unwrap_or_else(|e| {
                eprintln!(
                    "tzip: --raw-block metadata prefetch failed ({e:#}); \
                     falling back to per-file lookups"
                );
                (HashMap::new(), HashMap::new())
            });
            if verbose && !imap.is_empty() {
                eprintln!(
                    "tzip: --raw-block prefetched {} inodes, {} extent-lists in {:.2}s",
                    imap.len(),
                    emap.len(),
                    elapsed.as_secs_f64()
                );
            }
            (imap, emap)
        };

        Ok(Self {
            mount_point: mount_point.to_path_buf(),
            device: whole,
            pool_send,
            pool_recv,
            oid_cache: RwLock::new(HashMap::new()),
            inode_cache: RwLock::new(inode_map),
            extent_cache: RwLock::new(extent_map),
            block_cache,
            stats: ReadStats::default(),
            verbose,
        })
    }

    /// Look up a volume-relative path in the OID cache. On miss, resolves
    /// the parent directory (expensive one-time walk) and populates the
    /// cache with all of the parent's children in one `list_directory_by_oid`
    /// call. Reuses the caller's volume handle for both operations.
    fn resolve_oid(
        &self,
        vol: &mut ApfsVolume<AlignedRawReader>,
        rel_str: &str,
    ) -> Result<u64> {
        // Fast path — check for a direct hit. Bind the copied Option to a
        // variable so the RwLock read guard drops before we ever try to
        // take a write guard (`if let` scrutinee temporaries live to the
        // end of the whole if-let expression, which would deadlock the
        // write() call below on the same RwLock).
        let direct = self.oid_cache.read().unwrap().get(rel_str).copied();
        if let Some(oid) = direct {
            return Ok(oid);
        }
        let (parent_rel, basename) = split_parent_name(rel_str);

        // Look up parent — same read-guard-drop pattern as above.
        let parent_cached = self.oid_cache.read().unwrap().get(&parent_rel).copied();
        let parent_oid = if let Some(oid) = parent_cached {
            oid
        } else {
            let oid = vol
                .open_directory(&parent_rel)
                .map_err(|e| anyhow!("open_directory({parent_rel}): {e}"))?;
            self.oid_cache
                .write()
                .unwrap()
                .insert(parent_rel.clone(), oid);
            oid
        };

        // Names-only variant — populating the cache does not need the
        // per-child inode data that `list_directory_by_oid` fetches
        // (O(dir_size × btree_depth) block reads we'd throw away).
        let entries = vol
            .list_directory_names_by_oid(parent_oid)
            .map_err(|e| anyhow!("list_directory_names_by_oid({parent_oid}): {e}"))?;
        {
            let mut w = self.oid_cache.write().unwrap();
            let parent_norm = parent_rel.trim_end_matches('/');
            for (name, oid, _kind) in &entries {
                let full = if parent_norm.is_empty() {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", parent_norm, name)
                };
                w.insert(full, *oid);
            }
        }
        let looked_up = self.oid_cache.read().unwrap().get(rel_str).copied();
        looked_up.ok_or_else(|| {
            anyhow!(
                "{rel_str} not found in parent {parent_rel} (base {basename}) \
                 after list (dir has {} entries)",
                entries.len()
            )
        })
    }

    /// Walk `roots` recursively via the raw APFS parser and STREAM
    /// `WorkItem`s to `tx` as they are discovered. Replaces
    /// `bulk_walker::walk_stream_bulk` when `--raw-block` is on so we
    /// don't touch the VFS (no `getattrlistbulk`, no `openat`, no
    /// `stat`) — every syscall we avoid is one macOS Endpoint Security
    /// can't inspect.
    ///
    /// Along the way, populates the OID cache so the reader hot path
    /// remains a hashmap lookup. Fetches per-file inode data so each
    /// `WorkItem` carries an accurate `size` / `mtime`.
    ///
    /// Parallelized across roots via rayon: one volume per root drawn
    /// from the pool, sequential recursion within a root.
    pub fn walk_stream(
        &self,
        roots: &[PathBuf],
        exclude: &[String],
        tx: crossbeam_channel::Sender<WorkItem>,
    ) -> Result<()> {
        use rayon::prelude::*;

        let count = AtomicUsize::new(0);

        // IMPORTANT: the walker opens its OWN volume per root instead of
        // borrowing from the reader pool. A shared pool deadlocks: the
        // walker holds a volume for the whole (long) subtree walk, so
        // if enough walker workers grab all pool slots, readers block
        // on `pool_recv.recv()` forever, `feed_tx` fills, bridge blocks,
        // `wtx` fills, walker blocks on `tx.send` — hard deadlock.
        //
        // Opening a fresh volume costs ~50 ms of superblock parsing;
        // amortized over walking thousands of files it's noise.
        roots.par_iter().try_for_each(|root| -> Result<()> {
            let mut vol = open_volume(&self.device, self.block_cache.clone())?;
            self.walk_root(&mut vol, root, exclude, &tx, &count)
        })?;

        Ok(())
    }

    fn walk_root(
        &self,
        vol: &mut ApfsVolume<AlignedRawReader>,
        root: &Path,
        exclude: &[String],
        tx: &crossbeam_channel::Sender<WorkItem>,
        count: &AtomicUsize,
    ) -> Result<()> {
        // Convert to volume-relative.
        let rel_root = root
            .strip_prefix(&self.mount_point)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| root.to_path_buf());
        let rel_root_str = format!("/{}", rel_root.to_string_lossy().trim_start_matches('/'));

        // Resolve the root's OID + kind. Try as a directory first.
        let root_oid = match vol.open_directory(&rel_root_str) {
            Ok(o) => Some(o),
            Err(_) => None,
        };
        if let Some(oid) = root_oid {
            self.oid_cache
                .write()
                .unwrap()
                .insert(rel_root_str.clone(), oid);
            let base = root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.walk_dir(vol, root, &rel_root_str, oid, &base, exclude, tx, count)
        } else {
            // Root is a file. Emit a single item; the reader will fetch
            // the real size during its own inode lookup.
            let cached = self.oid_cache.read().unwrap().get(&rel_root_str).copied();
            let oid = match cached {
                Some(o) => o,
                None => match self.resolve_oid(vol, &rel_root_str) {
                    Ok(o) => o,
                    Err(_) => return Ok(()),
                },
            };
            let _ = oid;
            let name = root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if crate::bulk_walker::is_excluded(&name, exclude) {
                return Ok(());
            }
            let _ = tx.send(WorkItem {
                path: root.to_path_buf(),
                name_in_archive: name,
                size: 0,
                mtime: (((1 << 9) | (1 << 5)) as u16, 0),
                dirfd: None,
                basename: None,
            });
            count.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
    }

    /// Recurse into a directory, streaming files to `tx` as we go.
    /// Skips per-file inode lookups: `size` is set to 0 and mtime to the
    /// DOS epoch. The reader (`read_file_by_oid`) does its own inode
    /// lookup anyway, so paying the cost twice — once for the WorkItem
    /// and once for the read — was dominating wall time. Losing accurate
    /// size upfront costs us progress-bar precision and output preallocation
    /// accuracy, both fine trade-offs for a >10× walk speedup.
    #[allow(clippy::too_many_arguments)]
    fn walk_dir(
        &self,
        vol: &mut ApfsVolume<AlignedRawReader>,
        dir_abs: &Path,
        dir_rel: &str,
        dir_oid: u64,
        archive_prefix: &str,
        exclude: &[String],
        tx: &crossbeam_channel::Sender<WorkItem>,
        count: &AtomicUsize,
    ) -> Result<()> {
        let entries = vol
            .list_directory_names_by_oid(dir_oid)
            .map_err(|e| anyhow!("list_directory_names_by_oid({dir_oid}) for {dir_rel}: {e}"))?;

        // Populate OID cache in bulk and separate files from subdirs.
        let rel_norm = dir_rel.trim_end_matches('/');
        let mut children_files: Vec<(String, u64)> = Vec::new();
        let mut children_dirs: Vec<(String, u64)> = Vec::new();
        {
            let mut w = self.oid_cache.write().unwrap();
            for (name, child_oid, kind) in entries {
                let child_rel = if rel_norm.is_empty() {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", rel_norm, name)
                };
                w.insert(child_rel, child_oid);
                match kind {
                    EntryKind::File | EntryKind::Symlink => {
                        children_files.push((name, child_oid));
                    }
                    EntryKind::Directory => {
                        children_dirs.push((name, child_oid));
                    }
                }
            }
        }

        // Batch-fetch inodes ONLY when the children's OID range is tight
        // enough that one range scan is actually cheaper than N per-file
        // lookups. If the range spans a huge chunk of the tree (e.g. a
        // dir mixing files created months apart), the scan visits far
        // more leaves than N individual descents would touch and we lose.
        //
        // Heuristic: skip when max - min > 16 × N (each lookup ~5 leaf
        // reads worst case; scan visits ~(range / entries_per_leaf) leaves;
        // break-even is roughly range/10 ≈ 5N, so 16N gives comfortable
        // slack).
        if !children_files.is_empty() {
            let (min_oid, max_oid) = children_files.iter().map(|(_, o)| *o).fold(
                (u64::MAX, 0u64),
                |(mn, mx), o| (mn.min(o), mx.max(o)),
            );
            let n = children_files.len() as u64;
            let range = max_oid.saturating_sub(min_oid);
            let range_ok = range <= n.saturating_mul(16).max(64);
            if range_ok {
                if let Ok(inodes) = vol.batch_inodes_in_range(min_oid, max_oid) {
                    let mut w = self.inode_cache.write().unwrap();
                    for (oid, inode) in inodes {
                        w.insert(oid, inode);
                    }
                }
                // On error, the reader falls back to `lookup_inode` in
                // `read_file_by_oid` — correctness preserved.
            }
            // When the range is too wide, the reader's per-file
            // `lookup_inode` is the cheaper path; skip the batch.
        }

        // Emit files — no per-file inode lookup, size=0, DOS epoch mtime.
        let dos_epoch: (u16, u16) = (((1 << 9) | (1 << 5)) as u16, 0);
        for (name, _oid) in &children_files {
            let archive_name = if archive_prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", archive_prefix, name)
            };
            if crate::bulk_walker::is_excluded(&archive_name, exclude)
                || crate::bulk_walker::is_excluded(name, exclude)
            {
                continue;
            }
            let full_path = dir_abs.join(name);
            if tx
                .send(WorkItem {
                    path: full_path,
                    name_in_archive: archive_name,
                    size: 0,
                    mtime: dos_epoch,
                    dirfd: None,
                    basename: None,
                })
                .is_err()
            {
                return Ok(());
            }
            count.fetch_add(1, AtomicOrdering::Relaxed);
        }

        // Recurse into subdirs.
        for (name, oid) in &children_dirs {
            let child_prefix = if archive_prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", archive_prefix, name)
            };
            let child_rel = if rel_norm.is_empty() {
                format!("/{}", name)
            } else {
                format!("{}/{}", rel_norm, name)
            };
            let child_abs = dir_abs.join(name);
            self.walk_dir(
                vol,
                &child_abs,
                &child_rel,
                *oid,
                &child_prefix,
                exclude,
                tx,
                count,
            )?;
        }
        Ok(())
    }
}

/// Bulk (disk-extent-order) reader. Consumes a full item list, sorts
/// every file's extents by disk offset within bounded windows, and
/// serves them via coalesced sequential preads — turning ~2 lookups +
/// small random reads per file (~13 ms/file on a typical USB SSD) into
/// sequential-throughput I/O, which USB SSDs handle at ~10× the random
/// rate.
///
/// Trade-off vs. the per-file `Source::read` path: files complete in
/// disk order, not walker order. The pipeline downstream already sorts
/// by `RawItem::index` so this is transparent to the writer. Handled
/// out-of-band: symlinks and transparently compressed files (decmpfs)
/// fall back to `read_file_by_oid_with_inode` because their data isn't
/// in the extent tree.
impl RawApfsSource {
    /// Bulk-read `items` in disk-extent order. Emits `RawItem`s on
    /// `raw_tx` as files complete. Blocks the calling thread until
    /// every item is processed (or `raw_tx` is closed).
    ///
    /// Processes in `WINDOW` files at a time so memory stays bounded
    /// (only ~one window's worth of file buffers held simultaneously)
    /// while still capturing most sequential-locality wins within
    /// walker-adjacent files (files in a sibling series that were created
    /// together and land near each other on disk).
    pub fn bulk_read_all(
        &self,
        items: Vec<(u64, WorkItem)>,
        raw_tx: crossbeam_channel::Sender<crate::pipeline::RawItem>,
    ) -> Result<()> {
        /// Files per bulk-read window. Sized so per-window buffer
        /// footprint stays modest (~50-100 MB for typical file sizes)
        /// while still coalescing enough extents to make each pread
        /// meaningfully sequential.
        const WINDOW: usize = 512;
        /// Cap on any single coalesced pread. Modern USB SSDs sustain
        /// full throughput at 16-64 MiB reads; going bigger doesn't
        /// help and just balloons the read buffer.
        const MAX_RUN_BYTES: u64 = 64 * 1024 * 1024;

        // One volume for the bulk reader — a single-threaded loop.
        // Parallelizing across volumes is a future step; the fewer
        // syscalls / bigger reads shape usually beats it because the
        // drive itself serializes concurrent commands past ~8 anyway.
        let mut vol = match self.pool_recv.recv() {
            Ok(v) => v,
            Err(_) => open_volume(&self.device, self.block_cache.clone())?,
        };
        let block_size = vol.block_size() as u64;
        let mut run_buf: Vec<u8> = Vec::new();

        for window in items.chunks(WINDOW) {
            if self.bulk_read_window(&mut vol, window, block_size, MAX_RUN_BYTES, &mut run_buf, &raw_tx)? {
                // Downstream closed the channel — stop.
                let _ = self.pool_send.send(vol);
                return Ok(());
            }
        }
        let _ = self.pool_send.send(vol);
        Ok(())
    }

    /// Read one window's worth of files in disk-extent order. Returns
    /// `Ok(true)` if `raw_tx` was closed and we should stop, `Ok(false)`
    /// to continue.
    fn bulk_read_window(
        &self,
        vol: &mut ApfsVolume<AlignedRawReader>,
        window: &[(u64, WorkItem)],
        block_size: u64,
        max_run_bytes: u64,
        run_buf: &mut Vec<u8>,
        raw_tx: &crossbeam_channel::Sender<crate::pipeline::RawItem>,
    ) -> Result<bool> {
        // Per-window state: for every item, either a "special" flag
        // (symlink/compressed → fall back to point read) or the fully
        // allocated destination buffer + total remaining bytes to fill.
        // `sent` guards against double-shipping when the file also
        // qualifies for the Phase 3 empty-file / special-file paths.
        struct Slot {
            index: u64,
            item: WorkItem,
            data: Vec<u8>,
            remaining: u64,
            special: bool,
            sent: bool,
        }

        // Extent task: read `length` bytes starting at `disk_offset`,
        // copy them into slot `slot_idx` at file offset `file_offset`.
        struct ExtentTask {
            disk_offset: u64,
            length: u64,
            slot_idx: usize,
            file_offset: u64,
        }

        let mut slots: Vec<Slot> = Vec::with_capacity(window.len());
        let mut tasks: Vec<ExtentTask> = Vec::with_capacity(window.len() * 2);

        // Phase 1a: resolve every item's OID (from cache). Files whose
        // OID doesn't resolve are dropped from the window.
        let mut resolved: Vec<(u64, WorkItem, u64)> = Vec::with_capacity(window.len());
        for (idx, item) in window {
            let rel = item
                .path
                .strip_prefix(&self.mount_point)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| item.path.clone());
            let rel_str: String = format!("/{}", rel.to_string_lossy().trim_start_matches('/'));
            match self.resolve_oid(vol, &rel_str) {
                Ok(oid) => resolved.push((*idx, item.clone(), oid)),
                Err(_) => continue,
            }
        }

        // Phase 1b: batch-fetch inodes for the window's OID range, if
        // the range is tight enough that one scan is cheaper than N
        // individual descents. Guard: `range ≤ 32 × N` — a bit wider
        // than the walker's `16 × N` because ranges within a bulk
        // window tend to be tighter (walker-adjacent files) and the
        // scan reads through the shared block cache anyway. Populates
        // `inode_cache`, so the per-item loop below hits it.
        let t_meta = std::time::Instant::now();
        if !resolved.is_empty() {
            let (min_oid, max_oid) = resolved.iter().map(|(_, _, o)| *o).fold(
                (u64::MAX, 0u64),
                |(mn, mx), o| (mn.min(o), mx.max(o)),
            );
            let n = resolved.len() as u64;
            let range = max_oid.saturating_sub(min_oid);
            if range <= n.saturating_mul(32).max(64) {
                if let Ok(inodes) = vol.batch_inodes_in_range(min_oid, max_oid) {
                    let mut w = self.inode_cache.write().unwrap();
                    for (oid, inode) in inodes {
                        w.entry(oid).or_insert(inode);
                    }
                    self.stats.bulk_inode_batches.fetch_add(1, AtomicOrdering::Relaxed);
                } else {
                    self.stats.bulk_inode_batches_skipped.fetch_add(1, AtomicOrdering::Relaxed);
                }
            } else {
                self.stats.bulk_inode_batches_skipped.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }

        // Phase 1c: fetch inodes (from cache when possible), gather
        // private_ids, then batch-fetch every extent record for the
        // private_id range in one B-tree scan.
        let mut per_item: Vec<(u64, WorkItem, u64, apfs::catalog::InodeVal)> =
            Vec::with_capacity(resolved.len());
        let mut min_pid = u64::MAX;
        let mut max_pid = 0u64;
        for (idx, item, oid) in resolved {
            let cached_inode = self.inode_cache.read().unwrap().get(&oid).cloned();
            let inode = match cached_inode {
                Some(i) => i,
                None => match vol.lookup_inode_by_oid(oid) {
                    Ok(i) => {
                        self.inode_cache.write().unwrap().insert(oid, i.clone());
                        i
                    }
                    Err(_) => continue,
                },
            };
            let pid = inode.private_id;
            if pid < min_pid { min_pid = pid; }
            if pid > max_pid { max_pid = pid; }
            per_item.push((idx, item, oid, inode));
        }

        // Batch extents for the private_id range with the same guard.
        let mut extent_batch: HashMap<u64, Vec<(u64, apfs::catalog::FileExtentVal)>> =
            HashMap::new();
        if !per_item.is_empty() && min_pid <= max_pid {
            let n = per_item.len() as u64;
            let range = max_pid.saturating_sub(min_pid);
            if range <= n.saturating_mul(32).max(64) {
                if let Ok(exts) = vol.batch_extents_in_range(min_pid, max_pid) {
                    for (pid, logical_addr, val) in exts {
                        extent_batch
                            .entry(pid)
                            .or_default()
                            .push((logical_addr, val));
                    }
                    self.stats.bulk_extent_batches.fetch_add(1, AtomicOrdering::Relaxed);
                } else {
                    self.stats.bulk_extent_batches_skipped.fetch_add(1, AtomicOrdering::Relaxed);
                }
            } else {
                self.stats.bulk_extent_batches_skipped.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }
        let meta_elapsed = t_meta.elapsed();
        self.stats.bulk_meta_ns.fetch_add(meta_elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);

        // Phase 1d: build slots + extent tasks. Compressed / symlink
        // files are detected by inode flags / kind (no xattr lookup) —
        // xattr lookup would defeat the batched-metadata win, so we
        // check `bsd_flags & UF_COMPRESSED` (0x20) and fall back to
        // `vol.compression_header` only when the flag is ambiguous or
        // missing (rare).
        const UF_COMPRESSED: u32 = 0x0000_0020;
        for (idx, item, _oid, inode) in per_item {
            // Special-case: symlink or transparently compressed file.
            // Both live outside the extent tree so route through the
            // per-file fallback in Phase 3.
            let is_special = inode.kind() == apfs::catalog::INODE_SYMLINK_TYPE
                || (inode.bsd_flags & UF_COMPRESSED) != 0;
            if is_special {
                slots.push(Slot {
                    index: idx,
                    item,
                    data: Vec::new(),
                    remaining: 0,
                    special: true,
                    sent: false,
                });
                continue;
            }

            let file_size = inode.size();
            if file_size == 0 {
                slots.push(Slot {
                    index: idx,
                    item,
                    data: Vec::new(),
                    remaining: 0,
                    special: false,
                    sent: false,
                });
                continue;
            }

            // Preference order:
            //   1. Global extent_cache (populated by scan_all_metadata) — O(1).
            //   2. Per-window batch scan (if the window's pid range fit
            //      inside the guard).
            //   3. Per-file `lookup_extents` — only if everything above
            //      missed.
            let cached_extents = self.extent_cache.read().unwrap().get(&inode.private_id).cloned();
            let extent_recs: Vec<(u64, apfs::catalog::FileExtentVal)> = if let Some(v) = cached_extents {
                v
            } else if let Some(v) = extent_batch.remove(&inode.private_id) {
                v
            } else {
                match vol.lookup_extents_by_private_id(inode.private_id) {
                    Ok(recs) => recs
                        .into_iter()
                        .map(|r| (r.logical_addr, r.value))
                        .collect(),
                    Err(_) => {
                        slots.push(Slot {
                            index: idx,
                            item,
                            data: Vec::new(),
                            remaining: 0,
                            special: true,
                            sent: false,
                        });
                        continue;
                    }
                }
            };

            let slot_idx = slots.len();
            let data = vec![0u8; file_size as usize];
            let mut remaining = 0u64;

            for (logical_addr, val) in &extent_recs {
                let ext_len = val.length();
                let phys = val.phys_block_num * block_size;
                let file_off = *logical_addr;
                if file_off >= file_size {
                    continue;
                }
                let usable = ext_len.min(file_size - file_off);
                if usable == 0 {
                    continue;
                }
                tasks.push(ExtentTask {
                    disk_offset: phys,
                    length: usable,
                    slot_idx,
                    file_offset: file_off,
                });
                remaining += usable;
            }
            slots.push(Slot {
                index: idx,
                item,
                data,
                remaining,
                special: false,
                sent: false,
            });
        }

        // Phase 2: sort extent tasks by disk offset and coalesce
        // adjacent runs. Each run is one pread; each pread's bytes are
        // then sliced into the target slots.
        let t_scan = std::time::Instant::now();
        tasks.sort_by_key(|t| t.disk_offset);
        let scan_elapsed = t_scan.elapsed();
        self.stats.bulk_scan_ns.fetch_add(scan_elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.stats.bulk_extent_tasks.fetch_add(tasks.len() as u64, AtomicOrdering::Relaxed);
        let t_read = std::time::Instant::now();
        let mut window_run_bytes = 0u64;
        let mut window_runs = 0u64;

        let mut i = 0;
        while i < tasks.len() {
            let run_start = tasks[i].disk_offset;
            let mut run_end = run_start + tasks[i].length;
            let mut j = i + 1;
            while j < tasks.len() {
                let t = &tasks[j];
                // Coalesce if next task starts within the current run
                // (adjacent OR overlapping) AND the resulting run would
                // stay under the cap.
                if t.disk_offset <= run_end && (t.disk_offset + t.length - run_start) <= max_run_bytes {
                    run_end = run_end.max(t.disk_offset + t.length);
                    j += 1;
                } else if t.disk_offset > run_end
                    && (t.disk_offset + t.length - run_start) <= max_run_bytes
                    && (t.disk_offset - run_end) <= (256 * 1024)
                {
                    // Coalesce only over small gaps (≤256 KiB). Bigger
                    // gaps blow up read volume — a 32 MiB gap threshold
                    // on this dataset (files sparsely scattered) can
                    // multiply total bytes read by 5-10× and swamp any
                    // syscall-overhead savings.
                    run_end = t.disk_offset + t.length;
                    j += 1;
                } else {
                    break;
                }
            }

            // Align the pread boundaries to block_size for the raw
            // device.
            let aligned_start = run_start & !(block_size - 1);
            let aligned_end = ((run_end + block_size - 1) / block_size) * block_size;
            let aligned_len = (aligned_end - aligned_start) as usize;
            if run_buf.len() < aligned_len {
                run_buf.resize(aligned_len, 0);
            }
            vol.read_raw_at(aligned_start, &mut run_buf[..aligned_len])
                .with_context(|| format!("bulk pread {} bytes at {}", aligned_len, aligned_start))?;
            window_runs += 1;
            window_run_bytes += aligned_len as u64;

            // Slice each task's bytes out of the run buffer into its
            // slot; decrement remaining and ship the RawItem when zero.
            for t in &tasks[i..j] {
                let off_in_run = (t.disk_offset - aligned_start) as usize;
                let len = t.length as usize;
                let src = &run_buf[off_in_run..off_in_run + len];
                let slot = &mut slots[t.slot_idx];
                let dst_off = t.file_offset as usize;
                slot.data[dst_off..dst_off + len].copy_from_slice(src);
                slot.remaining -= t.length;
                if slot.remaining == 0 && !slot.special && !slot.sent {
                    let out = std::mem::take(&mut slot.data);
                    let item = std::mem::replace(
                        &mut slot.item,
                        WorkItem {
                            path: std::path::PathBuf::new(),
                            name_in_archive: String::new(),
                            size: 0,
                            mtime: (0, 0),
                            dirfd: None,
                            basename: None,
                        },
                    );
                    slot.sent = true;
                    if raw_tx
                        .send(crate::pipeline::RawItem {
                            index: slot.index,
                            item,
                            bytes: ReadBuf::Owned(out),
                        })
                        .is_err()
                    {
                        return Ok(true);
                    }
                }
            }
            i = j;
        }
        let read_elapsed = t_read.elapsed();
        self.stats.bulk_read_ns.fetch_add(read_elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.stats.bulk_coalesced_runs.fetch_add(window_runs, AtomicOrdering::Relaxed);
        self.stats.bulk_run_bytes.fetch_add(window_run_bytes, AtomicOrdering::Relaxed);

        let t_special = std::time::Instant::now();
        let mut window_specials = 0u64;
        // Phase 3: handle items not shipped by Phase 2 — empty files
        // (never had extents) and specials (symlinks / compressed).
        for slot in slots.iter_mut() {
            if slot.sent {
                continue;
            }
            if !slot.special && slot.remaining == 0 {
                // Empty file — no extents, nothing to fill.
                let item = std::mem::replace(
                    &mut slot.item,
                    WorkItem {
                        path: std::path::PathBuf::new(),
                        name_in_archive: String::new(),
                        size: 0,
                        mtime: (0, 0),
                        dirfd: None,
                        basename: None,
                    },
                );
                slot.sent = true;
                if raw_tx
                    .send(crate::pipeline::RawItem {
                        index: slot.index,
                        item,
                        bytes: ReadBuf::Owned(Vec::new()),
                    })
                    .is_err()
                {
                    return Ok(true);
                }
                continue;
            }
            if slot.special {
                window_specials += 1;
                // Small number of files; fall back to per-file read.
                let rel = slot
                    .item
                    .path
                    .strip_prefix(&self.mount_point)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| slot.item.path.clone());
                let rel_str: String =
                    format!("/{}", rel.to_string_lossy().trim_start_matches('/'));
                if let Ok(oid) = self.resolve_oid(vol, &rel_str) {
                    let bytes = vol.read_file_by_oid(oid).unwrap_or_default();
                    let item = std::mem::replace(
                        &mut slot.item,
                        WorkItem {
                            path: std::path::PathBuf::new(),
                            name_in_archive: String::new(),
                            size: 0,
                            mtime: (0, 0),
                            dirfd: None,
                            basename: None,
                        },
                    );
                    slot.sent = true;
                    if raw_tx
                        .send(crate::pipeline::RawItem {
                            index: slot.index,
                            item,
                            bytes: ReadBuf::Owned(bytes),
                        })
                        .is_err()
                    {
                        return Ok(true);
                    }
                }
            }
        }
        let special_elapsed = t_special.elapsed();
        self.stats.bulk_special_ns.fetch_add(special_elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.stats.bulk_special_files.fetch_add(window_specials, AtomicOrdering::Relaxed);
        self.stats.bulk_files.fetch_add(slots.len() as u64, AtomicOrdering::Relaxed);
        self.stats.bulk_windows.fetch_add(1, AtomicOrdering::Relaxed);

        Ok(false)
    }

}

impl Source for RawApfsSource {
    fn read(&self, item: &WorkItem) -> Result<ReadBuf> {
        let rel = item
            .path
            .strip_prefix(&self.mount_point)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| item.path.clone());
        let rel_str: String = format!("/{}", rel.to_string_lossy().trim_start_matches('/'));

        let t_total = std::time::Instant::now();
        let t_pool = std::time::Instant::now();
        let mut vol = match self.pool_recv.try_recv() {
            Ok(v) => v,
            Err(_) => match self.pool_recv.recv() {
                Ok(v) => v,
                Err(_) => open_volume(&self.device, self.block_cache.clone())?,
            },
        };
        let pool_wait = t_pool.elapsed();

        let t_resolve = std::time::Instant::now();
        let oid_result = self.resolve_oid(&mut vol, &rel_str);
        let resolve_time = t_resolve.elapsed();

        let (bytes_result, read_time) = match oid_result {
            Ok(oid) => {
                let t_read = std::time::Instant::now();
                // Fast path: if the walker pre-fetched this OID's inode
                // via `batch_inodes_in_range`, skip the reader's own
                // `lookup_inode` (one fewer B-tree walk per file).
                let cached_inode = self.inode_cache.read().unwrap().get(&oid).cloned();
                let br = match cached_inode {
                    Some(inode) => vol
                        .read_file_by_oid_with_inode(oid, &inode)
                        .map_err(|e| anyhow!("read_file_by_oid_with_inode({oid}) for {rel_str}: {e}")),
                    None => vol
                        .read_file_by_oid(oid)
                        .map_err(|e| anyhow!("read_file_by_oid({oid}) for {rel_str}: {e}")),
                };
                (br, t_read.elapsed())
            }
            Err(e) => (Err(e), std::time::Duration::ZERO),
        };
        let bytes_len = bytes_result.as_ref().map(|b| b.len()).unwrap_or(0);

        let _ = self.pool_send.send(vol);

        let total = t_total.elapsed();
        self.stats.record(bytes_len as u64, pool_wait, resolve_time, read_time, total);

        Ok(ReadBuf::Owned(bytes_result?))
    }
}

/// Split `/a/b/c/foo.dcm` into (`/a/b/c`, `foo.dcm`).
fn split_parent_name(rel: &str) -> (String, String) {
    match rel.rsplit_once('/') {
        Some(("", name)) => ("/".to_string(), name.to_string()),
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => ("/".to_string(), rel.to_string()),
    }
}

fn open_volume(
    device: &Path,
    cache: SharedBlockCacheRef,
) -> Result<ApfsVolume<AlignedRawReader>> {
    let f = OpenOptions::new()
        .read(true)
        .open(device)
        .with_context(|| {
            format!(
                "open {} — raw device is root:operator 0640; run as sudo or join operator",
                device.display()
            )
        })?;
    let reader = AlignedRawReader::new(f, 4096, cache);
    ApfsVolume::open(reader).map_err(|e| {
        anyhow!(
            "apfs::ApfsVolume::open({}) failed: {e}. If FileVault is enabled, \
             raw-block reading is unsupported — see docs/RAW_BLOCK.md.",
            device.display()
        )
    })
}

/// statfs the given path and return `/dev/diskNsM`.
fn bsd_device_for_mount(mount: &Path) -> Result<PathBuf> {
    let c = std::ffi::CString::new(mount.as_os_str().as_bytes())
        .map_err(|_| anyhow!("path contains NUL"))?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut sfs) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let name = unsafe { CStr::from_ptr(sfs.f_mntfromname.as_ptr()) };
    Ok(PathBuf::from(name.to_string_lossy().into_owned()))
}

/// `/dev/disk5s1` → `/dev/rdisk5` (whole disk, character device).
fn whole_disk_raw(bsd: &Path) -> Result<PathBuf> {
    let s = bsd.to_string_lossy();
    let base = s
        .strip_prefix("/dev/disk")
        .ok_or_else(|| anyhow!("unexpected device path: {s}"))?;
    let n_end = base
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(base.len());
    if n_end == 0 {
        return Err(anyhow!("no disk number in {s}"));
    }
    let n: &str = &base[..n_end];
    Ok(PathBuf::from(format!("/dev/rdisk{n}")))
}
