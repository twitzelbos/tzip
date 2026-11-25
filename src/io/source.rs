//! `Source` + `RawItem` + `LocalFsSource` — the read-stage seam that
//! sits BETWEEN the walker (which decides *what* to read) and the
//! encoder / writer (which encode what it got).
//!
//! Lives in its own module so the various read-stage implementations
//! (`LocalFsSource`, `DispatchIoSource`, `RawApfsBulkSource`,
//! `IoUringSource`) can depend on the trait without creating an edge
//! back to `pipeline`, which drove the lib-wide module cycle the
//! deslop-please lint flagged.

use anyhow::{Context, Result};

use crate::platform;
use crate::walker::WorkItem;

/// Abstract "give me the bytes for this item" — the seam where cloud
/// backends will plug in later. Trait objects live in an `Arc` so
/// multiple reader threads can share one instance without cloning
/// per-item state.
pub trait Source: Send + Sync {
    fn read(&self, item: &WorkItem) -> Result<platform::ReadBuf>;

    /// Read a whole chunk of items, sending each downstream as a
    /// `RawItem`. The default just loops `read()` in order; sources
    /// that benefit from reordering (extent-order / raw-APFS bulk
    /// readers) override this to sort by physical disk offset first.
    /// `index` is carried through untouched so the writer's ordering
    /// is unaffected.
    fn read_bulk(
        &self,
        items: Vec<(u64, WorkItem)>,
        raw_tx: crossbeam_channel::Sender<RawItem>,
    ) -> Result<()> {
        for (index, item) in items {
            let bytes = self.read(&item)?;
            if raw_tx.send(RawItem { index, item, bytes }).is_err() {
                break;
            }
        }
        Ok(())
    }
}

/// Read stage output — untouched bytes plus enough metadata to
/// identify the entry downstream.
pub struct RawItem {
    pub index: u64,
    pub item: WorkItem,
    pub bytes: platform::ReadBuf,
}

/// Standard `read(2)` / `pread(2)` source for local files on the
/// walker's own filesystem.
pub struct LocalFsSource {
    /// If true, keep read blocks in the OS page cache (default: bypass).
    pub keep_cache: bool,
}

impl Source for LocalFsSource {
    fn read(&self, item: &WorkItem) -> Result<platform::ReadBuf> {
        // Fast path: openat(dirfd, basename) when the walker gave us
        // a dirfd. This skips per-component path resolution — a real
        // win on deep directory layouts.
        if let (Some(dirfd), Some(basename)) = (&item.dirfd, &item.basename) {
            match platform::open_at(dirfd.as_raw(), basename) {
                Ok(f) => {
                    return platform::read_from_file(f, item.size, self.keep_cache)
                        .with_context(|| format!("read {}", item.path.display()));
                }
                Err(e) => {
                    if self.keep_cache {
                        eprintln!(
                            "openat failed for {}: {} — falling back to open by path",
                            item.path.display(),
                            e
                        );
                    }
                }
            }
        }
        platform::read_input(&item.path, item.size, self.keep_cache)
            .with_context(|| format!("read {}", item.path.display()))
    }
}
