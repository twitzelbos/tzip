//! Linux extent-order bulk reader — the `--raw-block` fast path on Linux.
//!
//! This is the Linux analog to the macOS `--raw-block` APFS reader. The macOS
//! version bypasses the VFS by parsing APFS off `/dev/rdiskN`; on Linux we
//! don't need a parser, because the kernel already exposes each file's
//! physical block layout through the `FS_IOC_FIEMAP` ioctl (see
//! [`crate::fiemap`]). Works on ext4, xfs, btrfs, f2fs.
//!
//! ## What it does
//!
//! The default reader pulls files in *walk* order — the order the directory
//! tree happened to be enumerated in. On a spinning disk or a USB-attached
//! drive that order is close to random with respect to physical layout, so
//! every file open triggers a seek. This reader instead:
//!
//! 1. Probes each file's first physical block offset via FIEMAP.
//! 2. Sorts a window of files into ascending disk order.
//! 3. Reads them in that order through the normal VFS, issuing readahead a
//!    few files ahead so the drive stays busy.
//!
//! The result is a monotonic forward sweep of the platters / flash instead of
//! a seek storm. On NVMe (no seek penalty) it's roughly neutral; the payoff
//! is on the media where seeks actually cost — exactly where the macOS
//! `--raw-block` path pays off too.
//!
//! Unlike the macOS raw-device path this still reads through the VFS, so it
//! does **not** bypass `fanotify`-based on-access AV. Defeating enterprise AV
//! on Linux is a separate problem (per-process exclusion); see the roadmap.

#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use std::fs::File;
use std::os::unix::io::AsRawFd;

use crate::fiemap;
use crate::pipeline::{RawItem, Source};
use crate::platform::{self, ReadBuf};
use crate::walker::WorkItem;

/// Files probed + sorted together before reading. Big enough to coalesce a
/// meaningful run into disk order, small enough that we hold at most this many
/// open fds and pending mmaps at once (comfortably under a 1024 RLIMIT_NOFILE).
const WINDOW: usize = 256;

/// How many files ahead of the current read to issue readahead for, so the
/// drive prefetches the next seek targets while we drain the current one.
const PREFETCH_DEPTH: usize = 4;

/// Only bother prefetching files at least this large — small files are
/// dominated by the seek that our ordering already eliminates, and priming
/// the cache for them just churns it.
const PREFETCH_MIN_BYTES: u64 = 256 * 1024;

pub struct LinuxExtentSource {
    /// Keep read blocks in the page cache (default: bypass so the archive
    /// pass doesn't evict the user's working set).
    pub keep_cache: bool,
    pub verbose: bool,
    /// Read file bodies via io_uring at high queue depth instead of one
    /// blocking read per file. Files are still submitted in physical-disk
    /// order (the FIEMAP sort below runs first).
    #[cfg(feature = "io-uring")]
    pub io_uring: bool,
}

impl LinuxExtentSource {
    /// Cheap probe: does FIEMAP work for `path`? Used to decide whether the
    /// extent-order reader is worth engaging (it's a no-op win on filesystems
    /// that don't implement it, e.g. NFS/tmpfs/overlay).
    pub fn fiemap_supported(path: &std::path::Path) -> bool {
        match File::open(path) {
            Ok(f) => fiemap::first_physical_offset(f.as_raw_fd()).is_some(),
            Err(_) => false,
        }
    }

    /// Open the file for an item, preferring the dirfd + basename fast path so
    /// we skip per-component path resolution (matches `LocalFsSource`).
    fn open_item(item: &WorkItem) -> Result<File> {
        if let (Some(dirfd), Some(basename)) = (&item.dirfd, &item.basename) {
            if let Ok(f) = platform::open_at(dirfd.as_raw(), basename) {
                return Ok(f);
            }
        }
        File::open(&item.path).with_context(|| format!("open {}", item.path.display()))
    }

    fn read_open(&self, f: File, item: &WorkItem) -> Result<ReadBuf> {
        platform::read_from_file(f, item.size, self.keep_cache)
            .with_context(|| format!("read {}", item.path.display()))
    }

    /// Drain `items`, reading them in physical-disk order in windows.
    ///
    /// `index` is carried through untouched, so the writer's `--sort`
    /// reordering (and the default arrival-order write) both keep working —
    /// we only change the *read* order, never the archive order.
    pub fn bulk_read_all(
        &self,
        items: Vec<(u64, WorkItem)>,
        raw_tx: crossbeam_channel::Sender<RawItem>,
    ) -> Result<()> {
        let mut probed_files: u64 = 0;
        let mut probed_hits: u64 = 0;

        for window in items.chunks(WINDOW) {
            // Pass 1: open each file and probe its first physical offset.
            // Files whose layout we can't get (unsupported fs, sparse/inline,
            // open failure) get u64::MAX so they sort to the end and read via
            // whatever fd we have (or the path fallback).
            let mut entries: Vec<Entry> = Vec::with_capacity(window.len());
            for (index, item) in window {
                match Self::open_item(item) {
                    Ok(f) => {
                        let phys = match fiemap::first_physical_offset(f.as_raw_fd()) {
                            Some(p) => {
                                probed_hits += 1;
                                p
                            }
                            None => u64::MAX,
                        };
                        probed_files += 1;
                        entries.push(Entry {
                            phys,
                            index: *index,
                            item: item.clone(),
                            file: Some(f),
                        });
                    }
                    Err(_) => {
                        // Defer the error to the read pass so it surfaces the
                        // same way the default reader would report it.
                        entries.push(Entry {
                            phys: u64::MAX,
                            index: *index,
                            item: item.clone(),
                            file: None,
                        });
                    }
                }
            }

            // Sort into disk order. Stable so same-offset files keep walk order.
            entries.sort_by(|a, b| a.phys.cmp(&b.phys));

            // Pass 2a: io_uring path — submit the whole (disk-ordered) window
            // and let the kernel keep many reads in flight.
            #[cfg(feature = "io-uring")]
            if self.io_uring {
                // Files that failed to open fall back to a path read; the rest
                // go through io_uring, mapped back to their entry index.
                let mut jobs: Vec<(std::os::unix::io::RawFd, u64)> = Vec::new();
                let mut job_to_entry: Vec<usize> = Vec::new();
                for (idx, e) in entries.iter().enumerate() {
                    match &e.file {
                        Some(f) => {
                            jobs.push((f.as_raw_fd(), e.item.size));
                            job_to_entry.push(idx);
                        }
                        None => {
                            let bytes = platform::read_input(
                                &e.item.path,
                                e.item.size,
                                self.keep_cache,
                            )
                            .with_context(|| format!("read {}", e.item.path.display()))?;
                            if raw_tx
                                .send(RawItem {
                                    index: e.index,
                                    item: e.item.clone(),
                                    bytes,
                                })
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                }
                let mut closed = false;
                crate::io_uring_src::read_batch(&jobs, |job, buf| {
                    let e = &entries[job_to_entry[job]];
                    if raw_tx
                        .send(RawItem {
                            index: e.index,
                            item: e.item.clone(),
                            bytes: ReadBuf::Owned(buf),
                        })
                        .is_err()
                    {
                        closed = true;
                        return false;
                    }
                    true
                })?;
                // `entries` (and its open fds) live until here; drop closes them.
                if closed {
                    return Ok(());
                }
                continue;
            }

            // Pass 2b: blocking read in disk order, prefetching a few files ahead.
            for i in 0..entries.len() {
                self.prefetch_ahead(&entries, i);

                let (index, item, file) = {
                    let e = &mut entries[i];
                    (e.index, e.item.clone(), e.file.take())
                };
                let bytes = match file {
                    Some(f) => self.read_open(f, &item)?,
                    None => platform::read_input(&item.path, item.size, self.keep_cache)
                        .with_context(|| format!("read {}", item.path.display()))?,
                };
                if raw_tx.send(RawItem { index, item, bytes }).is_err() {
                    return Ok(()); // downstream closed — stop early
                }
            }
        }

        if self.verbose {
            eprintln!(
                "tzip: extent-order reader probed {probed_files} files, \
                 {probed_hits} had a usable physical offset"
            );
        }
        Ok(())
    }

    /// Kick off kernel readahead for the next few (still-open, large-enough)
    /// files so their blocks are in flight by the time we reach them.
    fn prefetch_ahead(&self, entries: &[Entry], cur: usize) {
        let end = (cur + 1 + PREFETCH_DEPTH).min(entries.len());
        for e in &entries[cur + 1..end] {
            if e.item.size < PREFETCH_MIN_BYTES {
                continue;
            }
            if let Some(f) = &e.file {
                // Best-effort; ignore errors (e.g. fs without readahead).
                let n = e.item.size.min(i64::MAX as u64);
                unsafe {
                    libc::readahead(f.as_raw_fd(), 0, n as libc::size_t);
                }
            }
        }
    }
}

struct Entry {
    phys: u64,
    index: u64,
    item: WorkItem,
    file: Option<File>,
}

/// Best-effort read of the block device backing `path`: `(rotational,
/// removable)`, each `None` when the sysfs attribute can't be read (e.g. the
/// path is on tmpfs / NFS / a device-mapper stack with no simple backing
/// disk). Used by auto-tune to decide whether the extent-order reader is
/// worth engaging — the win is on seeking / removable media.
pub fn device_traits(path: &std::path::Path) -> (Option<bool>, Option<bool>) {
    use std::os::unix::ffi::OsStrExt;
    let cpath = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(cpath.as_ptr(), &mut st) } != 0 {
        return (None, None);
    }
    let dev = st.st_dev;
    let maj = libc::major(dev);
    let min = libc::minor(dev);

    // `/sys/dev/block/<maj>:<min>` symlinks to the device's sysfs dir, e.g.
    // `.../block/sda/sda1` for a partition. The `queue/` dir and `removable`
    // attribute live on the parent whole-disk node, so walk up until we find
    // `queue/rotational`.
    let start = std::path::PathBuf::from(format!("/sys/dev/block/{maj}:{min}"));
    let real = match std::fs::canonicalize(&start) {
        Ok(p) => p,
        Err(_) => return (None, None),
    };

    let read_flag = |p: &std::path::Path| -> Option<bool> {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| match s.trim() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            })
    };

    // Find the whole-disk dir (the one that has a `queue/` subdir).
    let mut disk = real.as_path();
    let rotational = loop {
        let q = disk.join("queue/rotational");
        if let Some(v) = read_flag(&q) {
            break Some(v);
        }
        match disk.parent() {
            Some(p) if p.starts_with("/sys") => disk = p,
            _ => break None,
        }
    };
    let removable = read_flag(&disk.join("removable"));
    (rotational, removable)
}

impl Source for LinuxExtentSource {
    fn read(&self, item: &WorkItem) -> Result<ReadBuf> {
        // Single-item path (used if the pipeline ever calls read() directly
        // rather than the bulk path): just openat + read, no reordering.
        let f = Self::open_item(item)?;
        self.read_open(f, item)
    }

    fn read_bulk(
        &self,
        items: Vec<(u64, WorkItem)>,
        raw_tx: crossbeam_channel::Sender<RawItem>,
    ) -> Result<()> {
        self.bulk_read_all(items, raw_tx)
    }
}
