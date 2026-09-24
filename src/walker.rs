//! Directory scan.
//!
//! On external / spinning drives, a jwalk-style parallel walk can already
//! cause seek storms. We keep the walker to a modest thread count, and let
//! the pipeline throttle reads separately from CPU work.

use anyhow::{Context, Result};
use jwalk::WalkDirGeneric;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::platform::OwnedDirFd;

#[derive(Clone)]
pub struct WorkItem {
    /// Absolute filesystem path (kept for logging + fallback open)
    pub path: PathBuf,
    /// Name inside the archive (forward-slash, no leading slash)
    pub name_in_archive: String,
    /// File size in bytes (from fs::metadata)
    pub size: u64,
    /// Modified time as MS-DOS (date, time) tuple
    pub mtime: (u16, u16),
    /// Optional shared directory fd. When present, the reader uses
    /// `openat(dirfd, basename)` to skip path resolution.
    pub dirfd: Option<Arc<OwnedDirFd>>,
    /// Basename component within `dirfd`, when `dirfd` is set.
    pub basename: Option<std::ffi::OsString>,
}

impl std::fmt::Debug for WorkItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkItem")
            .field("path", &self.path)
            .field("name_in_archive", &self.name_in_archive)
            .field("size", &self.size)
            .field("dirfd", &self.dirfd.as_ref().map(|d| d.as_raw()))
            .finish()
    }
}

pub struct WalkOpts<'a> {
    pub roots: &'a [PathBuf],
    pub exclude: &'a [String],
    pub sort: bool,
    pub walk_threads: usize,
}

/// Batch walk: collects the whole tree before returning. Used only for
/// `--sort` where deterministic ordering requires the full list up front.
pub fn walk(opts: WalkOpts) -> Result<Vec<WorkItem>> {
    let (tx, rx) = crossbeam_channel::unbounded::<WorkItem>();
    walk_stream(opts.roots, opts.exclude, opts.walk_threads, tx)?;
    let mut items: Vec<WorkItem> = rx.into_iter().collect();
    if opts.sort {
        items.sort_by(|a, b| a.name_in_archive.cmp(&b.name_in_archive));
    } else {
        items.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(items)
}

/// Streaming walk: sends every discovered `WorkItem` into `tx` as soon as
/// it's found. Returns when the walk is done and drops `tx`, which
/// terminates any downstream `recv` loop.
///
/// This is what unblocks the "cold-cache external drive" case — the reader
/// pool starts pulling files the instant the first directory listing lands,
/// instead of waiting for the entire tree to enumerate.
pub fn walk_stream(
    roots: &[PathBuf],
    exclude: &[String],
    walk_threads: usize,
    tx: crossbeam_channel::Sender<WorkItem>,
) -> Result<()> {
    for root in roots {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", root.display()))?;

        let base_dir = if root.is_file() {
            root.parent().unwrap_or(Path::new("")).to_path_buf()
        } else {
            root.clone()
        };
        let base_name = if root.is_file() {
            root.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            root.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "".to_string())
        };

        if root.is_file() {
            let meta = std::fs::metadata(&root)?;
            if !is_excluded(&base_name, exclude) {
                let item = build_item(&root, &base_name, &meta)?;
                if tx.send(item).is_err() {
                    return Ok(());
                }
            }
            continue;
        }

        // `read_metadata: true` (the jwalk default) means each entry's
        // metadata is filled during the readdir sweep — one fewer stat call
        // per file on Linux and a cache hit on macOS.
        let walker = WalkDirGeneric::<((), ())>::new(&root)
            .parallelism(jwalk::Parallelism::RayonNewPool(walk_threads.max(1)))
            .skip_hidden(false)
            .follow_links(false);

        for entry in walker {
            let entry = entry.with_context(|| format!("walk {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let full = entry.path();
            let rel = full.strip_prefix(&base_dir).unwrap_or(&full);
            let name = archive_name(&base_name, rel);
            if is_excluded(&name, exclude) {
                continue;
            }
            let meta = entry
                .metadata()
                .with_context(|| format!("stat {}", full.display()))?;
            let item = build_item(&full, &name, &meta)?;
            if tx.send(item).is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn build_item(path: &Path, name: &str, meta: &std::fs::Metadata) -> Result<WorkItem> {
    let mtime = dos_time_from_meta(meta);
    Ok(WorkItem {
        path: path.to_path_buf(),
        name_in_archive: name.to_string(),
        size: meta.len(),
        mtime,
        dirfd: None,
        basename: None,
    })
}

fn archive_name(base_name: &str, rel: &Path) -> String {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if base_name.is_empty() {
        rel_str.trim_start_matches('/').to_string()
    } else if rel_str.is_empty() {
        base_name.to_string()
    } else {
        format!("{}/{}", base_name, rel_str.trim_start_matches('/'))
    }
}

fn is_excluded(name: &str, globs: &[String]) -> bool {
    // Simple glob: only `*` supported for now. Full glob crate can slot in later.
    globs.iter().any(|g| glob_match(g, name))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    // Very small glob: `*` matches any run of chars, `?` matches one.
    // Anchored on both ends.
    fn recur(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                // try matching zero or more chars
                if recur(&p[1..], t) {
                    return true;
                }
                if let Some((_, rest)) = t.split_first() {
                    return recur(p, rest);
                }
                false
            }
            (Some(b'?'), Some(_)) => recur(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => recur(&p[1..], &t[1..]),
            _ => false,
        }
    }
    recur(pattern.as_bytes(), text.as_bytes())
}

fn dos_time_from_meta(meta: &std::fs::Metadata) -> (u16, u16) {
    use std::time::UNIX_EPOCH;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unix_to_dos(mtime)
}

fn unix_to_dos(unix: i64) -> (u16, u16) {
    // Convert Unix time to DOS (localtime not required for correctness; use UTC).
    // MS-DOS epoch is 1980-01-01. If we're before that, clamp.
    // Days since 1970-01-01
    let (secs, is_neg) = if unix < 0 { (0i64, true) } else { (unix, false) };
    let _ = is_neg;
    let days = secs / 86_400;
    let sec_of_day = (secs % 86_400) as u32;
    // Convert days-since-1970 to y/m/d via civil_from_days (Howard Hinnant).
    let (y, m, d) = civil_from_days(days);
    let year = y as i32;
    if year < 1980 {
        return (((1 << 9) | (1 << 5)) as u16, 0); // 1980-01-01 00:00:00
    }
    let dos_date =
        (((year - 1980) as u16) << 9) | ((m as u16) << 5) | (d as u16);
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day / 60) % 60;
    let sec2 = (sec_of_day % 60) / 2;
    let dos_time = ((hour as u16) << 11) | ((minute as u16) << 5) | (sec2 as u16);
    (dos_date, dos_time)
}

/// Convert days since 1970-01-01 to (year, month, day) using Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
