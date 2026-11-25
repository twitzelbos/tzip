//! On-disk index-entry cache — Phase 6 of the Elastic feature-parity
//! roadmap.
//!
//! Hot queries against a cold remote archive spend most of their wall
//! time re-fetching the same index entries over and over — the
//! `.tzip/names.v1` MPH table, `.tzip/terms.v1` posting lists, the
//! `.tzip/facets.v1/<field>` counters, the `.tzip/columnar.v1.parquet`
//! sidecar. These sentinel entries are the archive's "hot index" in
//! Elastic terms. Keeping them resident on the client turns a
//! repeated `tzip aggregate` over an S3 archive from "500 ms per
//! query cold" into "single-digit ms per query hot."
//!
//! ## On-disk layout
//!
//! ```text
//! <cache-dir>/v1/
//!     <archive_id>/
//!         _meta                 (human-readable: url + etag + size)
//!         <entry_name_hex>      (the raw body bytes)
//!         <entry_name_hex>
//!         ...
//! ```
//!
//! `archive_id` is a hex-encoded 32-byte SHA-256 of
//! `url || 0x00 || etag_or_len`. ETag change (archive replaced)
//! produces a new `archive_id` and leaves the old bucket to age out
//! via the LRU sweep — which doubles as invalidation.
//!
//! LRU order is tracked by file mtime. `get` touches the mtime to
//! `now` on hit; `sweep` enumerates everything, sorts by mtime, and
//! evicts oldest until total bytes ≤ `max_bytes`. Mtime isn't
//! perfect (some filesystems round to 1 s) but for cache entries
//! that live seconds-to-days the resolution is plenty.
//!
//! ## Two-tier layout (Phase 6b)
//!
//! A single in-process LRU sits in front of the on-disk cache so
//! repeat lookups during one `tzip` invocation (fan-out across shards,
//! back-to-back queries, the hot-reads inside a REPL-ish workflow)
//! don't pay even the local-SSD read cost. The RAM tier is pure
//! additive memoisation: every entry that lives in RAM also lives
//! (or just lived) on disk, so cross-process sharing still goes
//! through the disk layer like it did pre-6b. Two concurrent `tzip`
//! processes don't share RAM; they do share the disk cache.
//!
//! The RAM tier is byte-budgeted (default 64 MB), evicted LRU on
//! insert overflow. Individual entries larger than the budget
//! bypass RAM entirely and go straight to disk — otherwise one 100
//! MB Parquet sidecar would blow the whole cache away.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

const LAYOUT_VERSION: &str = "v1";

/// Default in-process RAM cache budget when the user doesn't set one
/// explicitly. 64 MB is "enough to hold a few typical Parquet
/// sidecars + every name / term / facet entry of the hot archives"
/// on the workloads we've benchmarked.
pub const DEFAULT_RAM_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// User-facing "where / how big" cache specification. Resolved from
/// `--cache-dir` + `--cache-size-mb` + `--cache-ram-mb` CLI flags;
/// passed to each read-side subcommand that should populate /
/// consult the cache.
#[derive(Clone, Debug)]
pub struct IndexCacheSpec {
    pub dir: PathBuf,
    pub max_bytes: u64,
    /// RAM-tier cap in bytes. `None` disables the RAM tier
    /// (disk-only mode, same as pre-Phase-6b behaviour).
    pub ram_max_bytes: Option<u64>,
}

impl IndexCacheSpec {
    /// Open (or create) the cache described by this spec and wrap it
    /// in an `Arc` so the Archive can hold on to it for as long as it
    /// needs.
    pub fn open(&self) -> Result<Arc<IndexCache>> {
        Ok(Arc::new(IndexCache::open_tiered(
            self.dir.clone(),
            self.max_bytes,
            self.ram_max_bytes,
        )?))
    }
}

/// Opaque per-archive identity. Derived from the archive's URL + a
/// content-change fingerprint (ETag preferred, Content-Length as a
/// fallback). Two archives at the same URL with different ETags get
/// distinct IDs; the stale one ages out through the LRU sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArchiveId(pub [u8; 32]);

impl ArchiveId {
    /// Hex encoding used as a path component inside the cache tree.
    pub fn as_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{:02x}", b);
        }
        s
    }

    /// Mix a URL and a content fingerprint into a stable archive ID.
    /// The caller picks the strongest fingerprint available — ETag
    /// is ideal; (Content-Length || Last-Modified) works when the
    /// server doesn't emit ETag; raw byte length is the weakest
    /// fallback.
    pub fn derive(url: &str, fingerprint: &str) -> Self {
        let mut h = Sha256::new();
        h.update(url.as_bytes());
        h.update(&[0u8]);
        h.update(fingerprint.as_bytes());
        let d = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&d);
        Self(out)
    }
}

/// Two-tier LRU over index-entry bodies: an in-process RAM cache
/// (bytes-budgeted, bypasses on entries bigger than its cap) sits
/// in front of an on-disk LRU. The disk layer is cross-process
/// shareable; the RAM layer is per-process. Every path through the
/// RAM tier is best-effort — if the lock is poisoned or the payload
/// is oversized, callers transparently fall through to disk.
///
/// A shared in-process mutex serialises `put` + disk-sweep so
/// concurrent CLI subcommands don't race on the eviction pass.
/// (Cross-process safety is advisory, same as any filesystem LRU:
/// readers just see whatever bytes are on disk.)
pub struct IndexCache {
    root: PathBuf,
    max_bytes: u64,
    sweep_lock: Mutex<()>,
    /// RAM tier. `None` when the user opted out (`ram_max_bytes =
    /// None` in the spec). The LRU is byte-budgeted — see
    /// `RamTier` for the eviction policy.
    ram: Option<Mutex<RamTier>>,
}

impl IndexCache {
    /// Open the cache with the on-disk tier only — pre-Phase-6b
    /// behaviour. New call sites use `open_tiered` to also enable
    /// the in-process RAM LRU; this one stays for existing tests
    /// and short-lived helpers that don't benefit from RAM.
    pub fn open(dir: PathBuf, max_bytes: u64) -> Result<Self> {
        Self::open_tiered(dir, max_bytes, None)
    }

    /// Open the cache with both tiers. `ram_max_bytes = Some(n)`
    /// enables the RAM LRU with an `n`-byte budget; `None` keeps
    /// RAM off (disk-only mode).
    pub fn open_tiered(
        dir: PathBuf,
        max_bytes: u64,
        ram_max_bytes: Option<u64>,
    ) -> Result<Self> {
        let root = dir.join(LAYOUT_VERSION);
        fs::create_dir_all(&root).with_context(|| {
            format!("create tzip index cache at {}", root.display())
        })?;
        let ram = ram_max_bytes.map(|cap| Mutex::new(RamTier::new(cap)));
        Ok(Self {
            root,
            max_bytes,
            sweep_lock: Mutex::new(()),
            ram,
        })
    }

    /// The default location: `$XDG_CACHE_HOME/tzip` on Linux,
    /// `~/Library/Caches/tzip` on macOS, falling back to
    /// `~/.cache/tzip` when neither env var is set.
    pub fn default_dir() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
            return Some(PathBuf::from(xdg).join("tzip"));
        }
        if cfg!(target_os = "macos") {
            if let Some(home) = std::env::var_os("HOME") {
                return Some(PathBuf::from(home).join("Library/Caches/tzip"));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(".cache/tzip"));
        }
        None
    }

    /// Look up one cached entry by (archive, entry-name). Checks
    /// RAM first (Phase 6b), falls through to disk on miss; on
    /// disk-hit promotes the payload into RAM for the next call.
    /// Disk hits also touch the file's mtime so subsequent LRU
    /// sweeps keep the entry.
    pub fn get(&self, id: &ArchiveId, entry_name: &str) -> Option<Vec<u8>> {
        let key = RamKey::new(*id, entry_name);
        if let Some(ram) = &self.ram {
            if let Some(bytes) = ram.lock().get(&key) {
                return Some(bytes.as_ref().clone());
            }
        }
        let path = self.entry_path(id, entry_name);
        let body = fs::read(&path).ok()?;
        let _ = touch(&path);
        if let Some(ram) = &self.ram {
            ram.lock().insert(key, Arc::new(body.clone()));
        }
        Some(body)
    }

    /// Store one entry's body in both tiers. The disk write is
    /// atomic (temp file + rename), so a crashed `tzip` never
    /// leaves half-written content behind. RAM insertion happens
    /// after the disk write succeeds; if the payload exceeds the
    /// RAM budget it bypasses RAM and lives on disk only.
    pub fn put(&self, id: &ArchiveId, entry_name: &str, bytes: &[u8]) -> Result<()> {
        let archive_dir = self.archive_dir(id);
        fs::create_dir_all(&archive_dir).with_context(|| {
            format!("create cache archive dir {}", archive_dir.display())
        })?;

        // Write the body and _meta atomically.
        atomic_write(&self.entry_path(id, entry_name), bytes)
            .context("write cached entry body")?;
        atomic_write(
            &archive_dir.join("_meta"),
            format!(
                "archive_id={}\nwritten_at={}\n",
                id.as_hex(),
                humantime_now()
            )
            .as_bytes(),
        )
        .ok();

        if let Some(ram) = &self.ram {
            ram.lock()
                .insert(RamKey::new(*id, entry_name), Arc::new(bytes.to_vec()));
        }

        // Serialise sweep across threads/subcommands within this
        // process. Readers aren't blocked — only the sweep is.
        let _guard = self.sweep_lock.lock();
        if let Ok(total) = self.scan_total_bytes() {
            if total > self.max_bytes {
                let _ = self.sweep_locked(total);
            }
        }
        Ok(())
    }

    /// Current RAM tier byte usage. Exposed for tests that assert
    /// the eviction policy is actually bounding memory.
    #[cfg(test)]
    pub(crate) fn ram_bytes_used(&self) -> u64 {
        self.ram.as_ref().map(|r| r.lock().used).unwrap_or(0)
    }

    /// Enumerate every cached file + mtime. Used by the sweep; also
    /// exported so tests can assert evictions precisely.
    pub fn enumerate_entries(&self) -> Result<Vec<CachedEntry>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for arch_dir in fs::read_dir(&self.root)? {
            let arch_dir = arch_dir?;
            if !arch_dir.file_type()?.is_dir() {
                continue;
            }
            for e in fs::read_dir(arch_dir.path())? {
                let e = e?;
                if !e.file_type()?.is_file() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "_meta" {
                    continue;
                }
                let meta = e.metadata()?;
                out.push(CachedEntry {
                    archive_dir: arch_dir.path(),
                    entry_file: e.path(),
                    size: meta.len(),
                    mtime: meta
                        .modified()
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
        Ok(out)
    }

    fn scan_total_bytes(&self) -> Result<u64> {
        Ok(self
            .enumerate_entries()?
            .iter()
            .map(|e| e.size)
            .sum())
    }

    fn sweep_locked(&self, mut total: u64) -> Result<()> {
        let mut entries = self.enumerate_entries()?;
        entries.sort_by_key(|e| e.mtime);
        for e in entries {
            if total <= self.max_bytes {
                break;
            }
            let _ = fs::remove_file(&e.entry_file);
            total = total.saturating_sub(e.size);
        }
        // Clean up archive dirs that became empty (apart from _meta).
        if let Ok(iter) = fs::read_dir(&self.root) {
            for arch_dir in iter.flatten() {
                let has_body = fs::read_dir(arch_dir.path())
                    .map(|rd| {
                        rd.flatten().any(|e| {
                            e.file_name().to_string_lossy() != "_meta"
                        })
                    })
                    .unwrap_or(false);
                if !has_body {
                    let _ = fs::remove_dir_all(arch_dir.path());
                }
            }
        }
        Ok(())
    }

    fn archive_dir(&self, id: &ArchiveId) -> PathBuf {
        self.root.join(id.as_hex())
    }

    fn entry_path(&self, id: &ArchiveId, entry_name: &str) -> PathBuf {
        self.archive_dir(id).join(hex_name(entry_name))
    }
}

/// Snapshot of one cached entry's on-disk record.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    pub archive_dir: PathBuf,
    pub entry_file: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

/// Hex-encode an entry name so it's filesystem-safe. Entry names
/// can contain `/` (paths like `.tzip/facets.v1/status_code`), so a
/// plain path join is not an option.
fn hex_name(entry_name: &str) -> String {
    let mut s = String::with_capacity(entry_name.len() * 2);
    for b in entry_name.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// RAM-tier cache key: an archive identity plus the entry-name
/// string. Heap-allocated so the LRU can own the key; cheap because
/// index entry names are short (`.tzip/columnar.v1.parquet` etc.).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RamKey {
    archive: ArchiveId,
    entry: String,
}

impl RamKey {
    fn new(archive: ArchiveId, entry: &str) -> Self {
        Self {
            archive,
            entry: entry.to_string(),
        }
    }
}

/// Byte-budgeted LRU for the RAM tier. The `lru` crate only
/// supports item-count budgets, but index entry sizes vary by three
/// orders of magnitude (small facet tables vs multi-MB Parquet
/// sidecars) — so a count budget would either waste RAM on tiny
/// entries or evict big ones unnecessarily. The map tracks
/// payloads, a parallel `order` VecDeque gives us LRU (push front
/// on insert, promote to front on hit, evict from back when over
/// budget).
///
/// Values are `Arc<Vec<u8>>` so handing a copy to the caller is
/// just an Arc clone + a single memcpy on dereference — no second
/// allocation in the critical path.
///
/// Entries larger than the whole RAM budget are dropped on insert
/// (no caching). Otherwise one 100 MB Parquet sidecar would blow
/// every smaller entry out of RAM.
struct RamTier {
    max_bytes: u64,
    used: u64,
    entries: std::collections::HashMap<RamKey, Arc<Vec<u8>>>,
    /// MRU-front ordering: push to front on insert/hit, pop from
    /// back on eviction.
    order: VecDeque<RamKey>,
}

impl RamTier {
    fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            used: 0,
            entries: std::collections::HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &RamKey) -> Option<Arc<Vec<u8>>> {
        let bytes = self.entries.get(key)?.clone();
        // Promote to MRU: a linear-scan remove is fine for our
        // scale (typical RAM tier holds dozens, not millions, of
        // entries — a Parquet sidecar at ~1 MB fills most of the
        // budget on its own).
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            if let Some(k) = self.order.remove(pos) {
                self.order.push_front(k);
            }
        }
        Some(bytes)
    }

    fn insert(&mut self, key: RamKey, value: Arc<Vec<u8>>) {
        let size = value.len() as u64;
        // Oversized entries bypass RAM: trying to fit one would
        // evict everything else and still probably not fit.
        if size > self.max_bytes {
            return;
        }
        // If the key already exists, drop its accounting first.
        if let Some(old) = self.entries.remove(&key) {
            self.used = self.used.saturating_sub(old.len() as u64);
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
        }
        self.entries.insert(key.clone(), value);
        self.order.push_front(key);
        self.used += size;
        while self.used > self.max_bytes {
            let Some(victim) = self.order.pop_back() else {
                break;
            };
            if let Some(dropped) = self.entries.remove(&victim) {
                self.used = self.used.saturating_sub(dropped.len() as u64);
            }
        }
    }
}

fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", target.display()))?;
    // `tempfile::NamedTempFile` in the parent dir, write, persist.
    let mut tmp = tempfile::Builder::new()
        .prefix(".tzip-cache-")
        .tempfile_in(parent)
        .with_context(|| format!("create temp cache file in {}", parent.display()))?;
    tmp.as_file_mut().write_all(bytes)?;
    tmp.as_file_mut().sync_all().ok();
    tmp.persist(target)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("persist {}: {}", target.display(), e.error))
}

fn touch(path: &Path) -> std::io::Result<()> {
    // Set mtime to now directly. `File::set_modified` is stable
    // since Rust 1.75 and routes to `futimens` (unix) /
    // `SetFileTime` (windows). Beats the append-zero-byte trick,
    // which is a no-op on APFS.
    let f = fs::OpenOptions::new().write(true).open(path)?;
    f.set_modified(SystemTime::now())
}

fn humantime_now() -> String {
    use std::time::UNIX_EPOCH;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}", secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cache(max_bytes: u64) -> (tempfile::TempDir, IndexCache) {
        let dir = tempfile::tempdir().unwrap();
        let c = IndexCache::open(dir.path().to_path_buf(), max_bytes).unwrap();
        (dir, c)
    }

    fn tmp_cache_tiered(
        max_bytes: u64,
        ram_bytes: u64,
    ) -> (tempfile::TempDir, IndexCache) {
        let dir = tempfile::tempdir().unwrap();
        let c =
            IndexCache::open_tiered(dir.path().to_path_buf(), max_bytes, Some(ram_bytes))
                .unwrap();
        (dir, c)
    }

    #[test]
    fn ram_tier_serves_second_get_without_disk_read() {
        // Prime the cache via put, then delete the on-disk file
        // behind its back. If the RAM tier is pulling its weight,
        // the next `get` returns the cached bytes from RAM; if it
        // isn't, the deletion is visible.
        let (_tmp, c) = tmp_cache_tiered(1_000_000, 1_000_000);
        let id = ArchiveId::derive("s3://b/k.zip", "etag-1");
        c.put(&id, ".tzip/names.v1", b"payload-from-ram").unwrap();
        // Delete the on-disk copy.
        let entry = c.entry_path(&id, ".tzip/names.v1");
        std::fs::remove_file(&entry).unwrap();
        // RAM still serves it.
        assert_eq!(
            c.get(&id, ".tzip/names.v1").as_deref(),
            Some(&b"payload-from-ram"[..])
        );
    }

    #[test]
    fn ram_tier_disabled_falls_back_to_disk_only() {
        // `open` (no ram tier) + delete-after-put must produce None
        // on the next get. Confirms the RAM path doesn't leak into
        // the one-tier constructor.
        let (_tmp, c) = tmp_cache(1_000_000);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        c.put(&id, ".tzip/names.v1", b"p").unwrap();
        let entry = c.entry_path(&id, ".tzip/names.v1");
        std::fs::remove_file(&entry).unwrap();
        assert!(c.get(&id, ".tzip/names.v1").is_none());
    }

    #[test]
    fn ram_tier_promotes_disk_hit() {
        // On a disk-only put (RAM tier empty), the first `get`
        // reads from disk AND populates RAM. Verified indirectly:
        // delete the file after the first get; the second get now
        // hits RAM.
        let (_tmp, c) = tmp_cache_tiered(1_000_000, 1_000_000);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        c.put(&id, ".tzip/names.v1", b"disk-first").unwrap();
        // Evict from RAM by rebuilding — easiest way to isolate
        // the "disk hit, then RAM promote" path.
        c.ram.as_ref().unwrap().lock().entries.clear();
        c.ram.as_ref().unwrap().lock().order.clear();
        c.ram.as_ref().unwrap().lock().used = 0;
        // First get: disk hit, RAM populated.
        assert_eq!(c.get(&id, ".tzip/names.v1").as_deref(), Some(&b"disk-first"[..]));
        // Now delete from disk; second get must still work via RAM.
        std::fs::remove_file(c.entry_path(&id, ".tzip/names.v1")).unwrap();
        assert_eq!(c.get(&id, ".tzip/names.v1").as_deref(), Some(&b"disk-first"[..]));
    }

    #[test]
    fn ram_tier_evicts_lru_on_overflow() {
        // 3 × 1 KB entries, 2 KB RAM budget → one eviction when
        // the third lands; LRU is the first-inserted.
        let (_tmp, c) = tmp_cache_tiered(1_000_000, 2048);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        let big = vec![0u8; 1024];
        c.put(&id, "first", &big).unwrap();
        c.put(&id, "second", &big).unwrap();
        c.put(&id, "third", &big).unwrap();
        // Delete all three from disk. The RAM cache now dictates
        // which keys survive.
        std::fs::remove_file(c.entry_path(&id, "first")).unwrap();
        std::fs::remove_file(c.entry_path(&id, "second")).unwrap();
        std::fs::remove_file(c.entry_path(&id, "third")).unwrap();
        assert!(c.get(&id, "first").is_none(), "LRU evicted first");
        assert!(c.get(&id, "second").is_some(), "second survives");
        assert!(c.get(&id, "third").is_some(), "third survives");
        assert!(c.ram_bytes_used() <= 2048);
    }

    #[test]
    fn ram_tier_bypasses_oversized_entries() {
        // A 10 KB entry into a 4 KB budget: it bypasses RAM and
        // goes straight to disk. Smaller entries in RAM stay.
        let (_tmp, c) = tmp_cache_tiered(1_000_000, 4096);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        c.put(&id, "small", &vec![0u8; 1024]).unwrap();
        c.put(&id, "huge", &vec![0u8; 10_000]).unwrap();
        // Delete both from disk. `small` should survive via RAM;
        // `huge` should not (it never made it into RAM).
        std::fs::remove_file(c.entry_path(&id, "small")).unwrap();
        std::fs::remove_file(c.entry_path(&id, "huge")).unwrap();
        assert!(c.get(&id, "small").is_some());
        assert!(c.get(&id, "huge").is_none());
    }

    #[test]
    fn ram_tier_promotes_on_get_so_lru_tracks_recency() {
        let (_tmp, c) = tmp_cache_tiered(1_000_000, 3 * 1024);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        let kb = vec![0u8; 1024];
        c.put(&id, "a", &kb).unwrap();
        c.put(&id, "b", &kb).unwrap();
        c.put(&id, "c", &kb).unwrap();
        // Touch "a" so it's now MRU.
        let _ = c.get(&id, "a");
        // Insert a fourth: "b" should go (it's now the LRU).
        c.put(&id, "d", &kb).unwrap();
        for name in ["a", "b", "c", "d"] {
            std::fs::remove_file(c.entry_path(&id, name)).unwrap();
        }
        assert!(c.get(&id, "a").is_some(), "a was touched → survives");
        assert!(c.get(&id, "b").is_none(), "b is LRU → evicted");
        assert!(c.get(&id, "c").is_some());
        assert!(c.get(&id, "d").is_some());
    }

    #[test]
    fn roundtrip_put_get() {
        let (_tmp, c) = tmp_cache(1_000_000);
        let id = ArchiveId::derive("s3://b/k.zip", "etag-123");
        c.put(&id, ".tzip/names.v1", b"payload").unwrap();
        assert_eq!(c.get(&id, ".tzip/names.v1").as_deref(), Some(&b"payload"[..]));
    }

    #[test]
    fn miss_returns_none() {
        let (_tmp, c) = tmp_cache(1_000_000);
        let id = ArchiveId::derive("s3://b/k.zip", "etag-123");
        assert!(c.get(&id, ".tzip/names.v1").is_none());
    }

    #[test]
    fn different_archives_do_not_collide() {
        let (_tmp, c) = tmp_cache(1_000_000);
        let a = ArchiveId::derive("s3://b/a.zip", "e1");
        let b = ArchiveId::derive("s3://b/b.zip", "e1");
        c.put(&a, ".tzip/names.v1", b"A").unwrap();
        c.put(&b, ".tzip/names.v1", b"B").unwrap();
        assert_eq!(c.get(&a, ".tzip/names.v1").as_deref(), Some(&b"A"[..]));
        assert_eq!(c.get(&b, ".tzip/names.v1").as_deref(), Some(&b"B"[..]));
    }

    #[test]
    fn etag_change_invalidates() {
        let (_tmp, c) = tmp_cache(1_000_000);
        let fresh = ArchiveId::derive("s3://b/k.zip", "etag-1");
        let stale = ArchiveId::derive("s3://b/k.zip", "etag-2");
        c.put(&fresh, ".tzip/names.v1", b"v1").unwrap();
        // Same URL, different ETag → different bucket → the old
        // bytes aren't visible under the new id.
        assert!(c.get(&stale, ".tzip/names.v1").is_none());
    }

    #[test]
    fn sweep_evicts_lru() {
        // Budget big enough for one "large" + the "small" entries
        // we'll touch but not enough for two large ones. After
        // writing two large ones, the older large one must be gone.
        let (_tmp, c) = tmp_cache(16 * 1024);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        // Write first large entry.
        let big1 = vec![0u8; 10 * 1024];
        c.put(&id, "entry/first", &big1).unwrap();
        // Keep mtime strictly ordered even on filesystems that
        // round to 1 s.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Write second large entry — now total > budget, sweep
        // runs, oldest (first) should be evicted.
        let big2 = vec![1u8; 10 * 1024];
        c.put(&id, "entry/second", &big2).unwrap();
        assert!(c.get(&id, "entry/first").is_none(), "LRU should evict first");
        assert_eq!(c.get(&id, "entry/second").as_deref(), Some(&big2[..]));
    }

    #[test]
    fn get_touches_mtime_for_lru_recency() {
        // Budget sized to hold 2/3 10 KB entries: 3×10 KB = 30 KB,
        // so budget 25 KB forces exactly one eviction when the
        // third goes in. That isolates "which entry gets picked"
        // to the LRU order itself.
        let (_tmp, c) = tmp_cache(25 * 1024);
        let id = ArchiveId::derive("s3://b/k.zip", "e1");
        let big = vec![0u8; 10 * 1024];
        c.put(&id, "first", &big).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        c.put(&id, "second", &big).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Touch `first` so it's now the most-recently-used.
        let _ = c.get(&id, "first");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Writing a third large entry forces a one-entry eviction;
        // `second` is the LRU, so it should go, not `first`.
        c.put(&id, "third", &big).unwrap();
        assert!(
            c.get(&id, "first").is_some(),
            "most-recently-touched entry must survive"
        );
        assert!(
            c.get(&id, "second").is_none(),
            "lru entry should be evicted"
        );
        assert!(c.get(&id, "third").is_some(), "freshest entry must survive");
    }

    #[test]
    fn default_dir_is_sane() {
        // HOME / XDG_CACHE_HOME based — just assert it produces
        // *something* sensible on this platform.
        let d = IndexCache::default_dir();
        assert!(d.is_some(), "default_dir should resolve on common OSes");
    }
}
