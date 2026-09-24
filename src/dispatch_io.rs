//! Grand Central Dispatch I/O reader.
//!
//! Wraps `dispatch_io_read` via the C shim in `dispatch_shim.c`. The channel
//! takes ownership of the file descriptor and closes it in its cleanup
//! handler; we never `close()` the fd from Rust after handing it over.

#![cfg(target_os = "macos")]

use anyhow::{Context, Result};
use std::os::unix::io::{IntoRawFd, RawFd};

use crate::pipeline::Source;
use crate::platform::{self, ReadBuf};
use crate::walker::WorkItem;

extern "C" {
    fn tzip_dispatch_read_all_sync(
        fd: libc::c_int,
        length: libc::size_t,
        out_buf: *mut u8,
        out_written: *mut libc::size_t,
    ) -> libc::c_int;
}

pub struct DispatchIoSource {
    pub keep_cache: bool,
}

impl Source for DispatchIoSource {
    fn read(&self, item: &WorkItem) -> Result<ReadBuf> {
        // Large files → mmap; dispatch_io doesn't beat zero-copy.
        if item.size >= platform::MMAP_THRESHOLD {
            let file = if let (Some(dirfd), Some(basename)) = (&item.dirfd, &item.basename) {
                platform::open_at(dirfd.as_raw(), basename)
                    .or_else(|_| std::fs::OpenOptions::new().read(true).open(&item.path))
            } else {
                std::fs::OpenOptions::new().read(true).open(&item.path)
            }
            .with_context(|| format!("open {}", item.path.display()))?;
            return platform::read_from_file(file, item.size, self.keep_cache)
                .with_context(|| format!("read {}", item.path.display()));
        }

        // Small file → GCD path
        let file = if let (Some(dirfd), Some(basename)) = (&item.dirfd, &item.basename) {
            platform::open_at(dirfd.as_raw(), basename)
                .or_else(|_| std::fs::OpenOptions::new().read(true).open(&item.path))
        } else {
            std::fs::OpenOptions::new().read(true).open(&item.path)
        }
        .with_context(|| format!("open {}", item.path.display()))?;

        let fd: RawFd = file.into_raw_fd();
        // dispatch_io channel now owns the fd; do NOT close from Rust.

        let len = item.size as usize;
        let mut buf: Vec<u8> = vec![0u8; len];
        let mut written: libc::size_t = 0;

        let rc = unsafe {
            tzip_dispatch_read_all_sync(fd, len, buf.as_mut_ptr(), &mut written)
        };
        if rc != 0 {
            anyhow::bail!(
                "dispatch_io read failed for {}: errno {}",
                item.path.display(),
                rc
            );
        }
        buf.truncate(written);
        Ok(ReadBuf::Owned(buf))
    }
}
