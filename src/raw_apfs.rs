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
    /// Inode data cache keyed by OID, populated by the walker via one
    /// B-tree range scan per directory instead of N `lookup_inode`s.
    /// Reader hot path clones the inode out and hands it to
    /// `read_file_by_oid_with_inode`, saving one B-tree walk per file.
    inode_cache: RwLock<HashMap<u64, InodeVal>>,
    /// Block cache shared across every `ApfsVolume` reader on this
    /// source — interior B-tree nodes are read once and served from
    /// RAM to every reader thread. Sharded so N readers on different
    /// offsets don't contend on a single lock.
    block_cache: SharedBlockCacheRef,
    stats: ReadStats,
}

/// Accumulated per-stage cost of the reader hot path. Dumped at `Drop`.
#[derive(Default)]
pub struct ReadStats {
    files: std::sync::atomic::AtomicU64,
    total_bytes: std::sync::atomic::AtomicU64,
    pool_wait_ns: std::sync::atomic::AtomicU64,
    resolve_ns: std::sync::atomic::AtomicU64,
    read_ns: std::sync::atomic::AtomicU64,
    total_ns: std::sync::atomic::AtomicU64,
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
        let n = self.stats.files.load(Relaxed);
        if n == 0 {
            return;
        }
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
}

impl RawApfsSource {
    /// Open `pool_size` independent readers against the APFS container
    /// backing `mount_point`. Requires root or `operator` group.
    pub fn open_for_mount(mount_point: &Path, pool_size: usize) -> Result<Self> {
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

        Ok(Self {
            mount_point: mount_point.to_path_buf(),
            device: whole,
            pool_send,
            pool_recv,
            oid_cache: RwLock::new(HashMap::new()),
            inode_cache: RwLock::new(HashMap::new()),
            block_cache,
            stats: ReadStats::default(),
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

    /// Walk `roots` (absolute paths — must all be under `self.mount_point`)
    /// recursively, populating the OID cache with every descendant. After
    /// this returns, `resolve_oid` on any file in the walked subtree is
    /// a pure hashmap lookup — the reader hot path does zero B-tree work.
    ///
    /// Roots are walked in parallel across the volume pool: one volume per
    /// root, up to the pool size. Directories are walked sequentially
    /// within a root (to keep each volume's fd on one seek arm at a time)
    /// but multiple roots run concurrently on separate fds.
    ///
    /// Returns the number of entries added to the cache.
    pub fn prewalk<P: AsRef<Path>>(&self, roots: &[P]) -> Result<usize> {
        use rayon::prelude::*;
        let count = AtomicUsize::new(0);

        // Normalize + de-dup + convert to volume-relative.
        let mut rel_roots: Vec<String> = Vec::with_capacity(roots.len());
        for r in roots {
            let stripped = r
                .as_ref()
                .strip_prefix(&self.mount_point)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| r.as_ref().to_path_buf());
            let rel = format!("/{}", stripped.to_string_lossy().trim_start_matches('/'));
            rel_roots.push(rel);
        }
        rel_roots.sort();
        rel_roots.dedup();

        rel_roots.par_iter().try_for_each(|root| -> Result<()> {
            let mut vol = match self.pool_recv.recv() {
                Ok(v) => v,
                Err(_) => open_volume(&self.device, self.block_cache.clone())?,
            };
            let res = self.prewalk_from(&mut vol, root, &count);
            let _ = self.pool_send.send(vol);
            res
        })?;

        Ok(count.load(AtomicOrdering::Relaxed))
    }

    /// Ensure `rel_str`'s OID is cached, then if it's a directory recurse
    /// into it (populating child OIDs). No-op if the path isn't in the
    /// volume — silently skip so a bogus CLI path doesn't kill the prewalk.
    fn prewalk_from(
        &self,
        vol: &mut ApfsVolume<AlignedRawReader>,
        rel_str: &str,
        count: &AtomicUsize,
    ) -> Result<()> {
        // Resolve the root. Cache the OID either way; recursion depends
        // on whether it's a directory. Bind the Option first so the read
        // guard drops before we ever try to take a write guard — a match
        // scrutinee holds its temporaries for the whole match, which
        // would deadlock the write below.
        let cached = self.oid_cache.read().unwrap().get(rel_str).copied();
        let oid = match cached {
            Some(o) => o,
            None => {
                // Try as a directory first; if it's a file, `open_directory`
                // returns NotADirectory — fall back to `resolve_oid`.
                match vol.open_directory(rel_str) {
                    Ok(o) => {
                        self.oid_cache
                            .write()
                            .unwrap()
                            .insert(rel_str.to_string(), o);
                        o
                    }
                    Err(_) => {
                        // Not a directory — treat as a file and populate via
                        // the standard resolve. Don't recurse.
                        let _ = self.resolve_oid(vol, rel_str);
                        return Ok(());
                    }
                }
            }
        };

        // List names + oids of children.
        let entries = match vol.list_directory_names_by_oid(oid) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };

        // Cache all children and collect subdirectories for recursion.
        let rel_norm = rel_str.trim_end_matches('/');
        let mut subdirs: Vec<String> = Vec::new();
        {
            let mut w = self.oid_cache.write().unwrap();
            for (name, child_oid, kind) in &entries {
                let full = if rel_norm.is_empty() {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", rel_norm, name)
                };
                w.insert(full.clone(), *child_oid);
                count.fetch_add(1, AtomicOrdering::Relaxed);
                if matches!(kind, EntryKind::Directory) {
                    subdirs.push(full);
                }
            }
        }

        for sub in &subdirs {
            self.prewalk_from(vol, sub, count)?;
        }
        Ok(())
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
