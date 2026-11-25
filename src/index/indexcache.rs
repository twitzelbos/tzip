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
//! ## Why on-disk, not RAM
//!
//! On modern SSDs a 1 MB read is sub-millisecond; the index entries
//! we care about are typically 100 KB – 50 MB. A RAM tier would
//! halve the cost on very hot paths, but it's a doubled complexity
//! surface (concurrent lock on an in-process map, cross-process
//! invalidation, memory budget separate from the disk budget) for a
//! sub-millisecond win. Cross-process sharing is the bigger
//! practical gain — two concurrent `tzip` invocations against the
//! same archive see each other's cache for free.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const LAYOUT_VERSION: &str = "v1";

/// User-facing "where / how big" cache specification. Resolved from
/// `--cache-dir` + `--cache-size-mb` CLI flags; passed to each
/// read-side subcommand that should populate / consult the cache.
#[derive(Clone, Debug)]
pub struct IndexCacheSpec {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

impl IndexCacheSpec {
    /// Open (or create) the cache described by this spec and wrap it
    /// in an `Arc` so the Archive can hold on to it for as long as it
    /// needs.
    pub fn open(&self) -> Result<std::sync::Arc<IndexCache>> {
        Ok(std::sync::Arc::new(IndexCache::open(self.dir.clone(), self.max_bytes)?))
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

/// On-disk LRU over index-entry bodies.
///
/// Cheap to clone — all state is behind the filesystem; the struct
/// itself is only the root path and the size budget. A shared
/// in-process mutex serialises `put` + `sweep` so concurrent CLI
/// subcommands don't race on the eviction pass. (Cross-process
/// safety is advisory, same as any filesystem LRU: readers just
/// see whatever bytes are on disk.)
pub struct IndexCache {
    root: PathBuf,
    max_bytes: u64,
    sweep_lock: Mutex<()>,
}

impl IndexCache {
    /// Create (or reuse) a cache rooted at `dir`. The directory is
    /// created on first use; subsequent opens reuse it. `max_bytes`
    /// caps total on-disk size.
    pub fn open(dir: PathBuf, max_bytes: u64) -> Result<Self> {
        let root = dir.join(LAYOUT_VERSION);
        fs::create_dir_all(&root).with_context(|| {
            format!("create tzip index cache at {}", root.display())
        })?;
        Ok(Self {
            root,
            max_bytes,
            sweep_lock: Mutex::new(()),
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

    /// Look up one cached entry by (archive, entry-name). On hit,
    /// the file's mtime is touched to `now` so LRU sweeps keep it.
    pub fn get(&self, id: &ArchiveId, entry_name: &str) -> Option<Vec<u8>> {
        let path = self.entry_path(id, entry_name);
        let body = fs::read(&path).ok()?;
        // Touch mtime on hit so subsequent sweeps keep this entry.
        // `set_file_times` is unix-only via FileExt; fall back to
        // opening O_WRONLY|O_APPEND which also updates mtime.
        let _ = touch(&path);
        Some(body)
    }

    /// Store one entry's body in the cache. Writes to a temp file
    /// and renames, so a crashed `tzip` never leaves half-written
    /// cache content behind. Triggers a sweep if the archive's
    /// bucket crosses the budget (cheap size check on the bucket
    /// dir first).
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
