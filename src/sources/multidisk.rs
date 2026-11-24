//! Multi-disk (spanned) ZIP archives — read-only.
//!
//! The classic "split across floppies" format: an archive whose central
//! directory and bodies are laid out across several files named
//! `base.z01`, `base.z02`, ... `base.z(N-1)`, with the last disk ending
//! in `base.zip`. The last disk holds the EOCD trailer; CDR entries
//! carry a `disk_number_start` + a disk-relative `lfh_offset`.
//!
//! This module exposes the N on-disk files as one virtual byte stream.
//! `Archive::open()` detects multi-disk from the EOCD's `this_disk`
//! field, opens the sibling files from the filesystem, and transforms
//! `(disk, offset)` pairs to absolute offsets in the concatenated
//! address space. Everything downstream — LFH probes, body streaming,
//! grep fan-out — then runs unchanged.
//!
//! Supports local files only. Multi-disk over HTTP/S3 isn't in scope:
//! the sibling disks would need to be addressable by URL, which no
//! standard cloud-archive layout handles.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::sources::ReadSeek;

/// A read-seek source over one or more ZIP disks presented as a
/// contiguous byte stream.
pub struct MultiDiskSource {
    files: Vec<File>,
    /// Cumulative offset at the start of each disk. `starts[0] == 0`.
    starts: Vec<u64>,
    /// Per-disk length in bytes, captured at `open()` time. Avoids an
    /// `fstat` per `Read::read` — a large-entry extract would otherwise
    /// trigger one per 8 KiB buffered chunk.
    sizes: Vec<u64>,
    /// Last-known file-position of each disk's fd, so we can skip
    /// `lseek` on sequential reads within one disk (the common case).
    /// Starts at the fd's initial offset (0 — the file was just
    /// opened); updated after every successful seek or read.
    fd_pos: Vec<u64>,
    total: u64,
    pos: u64,
}

impl MultiDiskSource {
    /// Open an archive where `last_path` is the final disk (the one
    /// with the EOCD). Opens `last_path`'s siblings `.z01` ..
    /// `.z(total_disks-1)` in order; then `last_path` as the final disk.
    /// `total_disks` is `eocd.this_disk + 1`.
    pub fn open(last_path: &Path, total_disks: usize) -> Result<Self> {
        if total_disks < 2 {
            bail!(
                "MultiDiskSource::open called with total_disks={} — use File::open directly",
                total_disks
            );
        }

        let base = last_path.with_extension(""); // strips .zip (or any ext)
        let mut files: Vec<File> = Vec::with_capacity(total_disks);
        let mut starts: Vec<u64> = Vec::with_capacity(total_disks);
        let mut sizes: Vec<u64> = Vec::with_capacity(total_disks);
        let mut total: u64 = 0;

        for i in 0..total_disks - 1 {
            let path = disk_path(&base, i);
            let f = File::open(&path).with_context(|| {
                format!(
                    "open multi-disk archive member {}: {}",
                    i,
                    path.display()
                )
            })?;
            let size = f.metadata()?.len();
            starts.push(total);
            sizes.push(size);
            total = total
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("multi-disk archive total exceeds u64"))?;
            files.push(f);
        }
        let f = File::open(last_path).with_context(|| {
            format!("open multi-disk last member: {}", last_path.display())
        })?;
        let size = f.metadata()?.len();
        starts.push(total);
        sizes.push(size);
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("multi-disk archive total exceeds u64"))?;
        files.push(f);

        let fd_pos = vec![0u64; total_disks];
        Ok(Self {
            files,
            starts,
            sizes,
            fd_pos,
            total,
            pos: 0,
        })
    }

    /// Cumulative start offset of each disk in the concatenated stream.
    /// `disk_starts()[d] + disk_relative_offset` yields the absolute
    /// offset within the virtual stream.
    pub fn disk_starts(&self) -> &[u64] {
        &self.starts
    }

    /// Resolve a byte position to `(disk_index, in_disk_offset)`.
    fn resolve(&self, pos: u64) -> Option<(usize, u64)> {
        if pos >= self.total {
            return None;
        }
        // Linear scan is fine; archives rarely exceed a handful of disks.
        let mut disk = 0usize;
        for (i, &s) in self.starts.iter().enumerate() {
            if s <= pos {
                disk = i;
            } else {
                break;
            }
        }
        Some((disk, pos - self.starts[disk]))
    }
}

/// Construct the `.zNN` path for disk index `i` (0-based disk number,
/// so `i=0` → `.z01`, matching the ZIP naming convention). `base` is
/// the archive path with its extension stripped.
fn disk_path(base: &Path, i: usize) -> PathBuf {
    // Info-ZIP produces `.z01`..`.z99`, then `.z100`..; `{:02}` gives
    // that layout (2-digit padding below 100, natural width above).
    let ext = format!("z{:02}", i + 1);
    base.with_extension(ext)
}

impl Read for MultiDiskSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let Some((disk, in_disk)) = self.resolve(self.pos) else {
            return Ok(0); // At or past EOF
        };
        // Use the per-disk size captured at open() instead of an fstat
        // per call — sequential extract/grep used to pay one metadata
        // syscall per buffered chunk.
        let remaining_in_disk = self.sizes[disk].saturating_sub(in_disk);
        if remaining_in_disk == 0 {
            return Ok(0);
        }
        // Skip the lseek when the fd is already positioned where we
        // want to read — the common case for sequential reads within
        // one disk, which is most of what the ZIP body stream looks
        // like. Short reads at disk boundaries stay the caller's
        // problem (`read_exact` + `std::io::copy` already loop).
        if self.fd_pos[disk] != in_disk {
            self.files[disk].seek(SeekFrom::Start(in_disk))?;
            self.fd_pos[disk] = in_disk;
        }
        let want = (remaining_in_disk.min(out.len() as u64)) as usize;
        let n = self.files[disk].read(&mut out[..want])?;
        self.pos += n as u64;
        self.fd_pos[disk] += n as u64;
        Ok(n)
    }
}

impl Seek for MultiDiskSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos: i64 = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.total as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative offset",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

// Confirm the trait bounds that the rest of tzip relies on. `File` is
// Send; `Vec<File>` is Send; so `MultiDiskSource` is Send automatically.
fn _assert_readseek()
where
    MultiDiskSource: ReadSeek,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    /// Write three tiny files, name them so disk_path() picks them up,
    /// and verify the concatenated source yields the right bytes
    /// end-to-end.
    #[test]
    fn concatenates_three_disks() {
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("arch");
        let d1 = tmp.path().join("arch.z01");
        let d2 = tmp.path().join("arch.z02");
        let last = tmp.path().join("arch.zip");
        File::create(&d1).unwrap().write_all(b"aaaa").unwrap();
        File::create(&d2).unwrap().write_all(b"bbbb").unwrap();
        File::create(&last).unwrap().write_all(b"cccc").unwrap();
        let _ = base; // just for symmetry

        let mut md = MultiDiskSource::open(&last, 3).unwrap();
        let mut all = Vec::new();
        md.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"aaaabbbbcccc");
        assert_eq!(md.disk_starts(), &[0, 4, 8]);
    }

    #[test]
    fn seeks_across_disk_boundary() {
        let tmp = tempdir().unwrap();
        let d1 = tmp.path().join("arch.z01");
        let last = tmp.path().join("arch.zip");
        File::create(&d1).unwrap().write_all(b"01234567").unwrap(); // 8 bytes
        File::create(&last).unwrap().write_all(b"ABCDEFGH").unwrap(); // 8 bytes

        let mut md = MultiDiskSource::open(&last, 2).unwrap();
        md.seek(SeekFrom::Start(6)).unwrap();
        let mut buf = [0u8; 6];
        // 6..12 spans disk boundary. First read returns 2 bytes ("67"),
        // next read returns up to 4 bytes ("ABCD").
        let n1 = md.read(&mut buf).unwrap();
        assert_eq!(&buf[..n1], b"67");
        let n2 = md.read(&mut buf[n1..]).unwrap();
        assert_eq!(&buf[n1..n1 + n2], b"ABCD");
    }

    #[test]
    fn read_exact_handles_disk_boundary() {
        let tmp = tempdir().unwrap();
        let d1 = tmp.path().join("arch.z01");
        let last = tmp.path().join("arch.zip");
        File::create(&d1).unwrap().write_all(b"01234567").unwrap();
        File::create(&last).unwrap().write_all(b"ABCDEFGH").unwrap();

        let mut md = MultiDiskSource::open(&last, 2).unwrap();
        md.seek(SeekFrom::Start(6)).unwrap();
        let mut buf = [0u8; 6];
        md.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"67ABCD");
    }

    #[test]
    fn disk_path_naming_matches_info_zip() {
        let base = Path::new("/tmp/bigarchive");
        assert_eq!(disk_path(base, 0), PathBuf::from("/tmp/bigarchive.z01"));
        assert_eq!(disk_path(base, 8), PathBuf::from("/tmp/bigarchive.z09"));
        assert_eq!(disk_path(base, 98), PathBuf::from("/tmp/bigarchive.z99"));
        assert_eq!(disk_path(base, 99), PathBuf::from("/tmp/bigarchive.z100"));
    }

    #[test]
    fn missing_disk_errors() {
        let tmp = tempdir().unwrap();
        let last = tmp.path().join("arch.zip");
        File::create(&last).unwrap().write_all(b"zip").unwrap();
        // .z01 doesn't exist — opening with 2 disks should fail.
        let err = match MultiDiskSource::open(&last, 2) {
            Ok(_) => panic!("expected .z01 to be missing"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("multi-disk"), "unhelpful err: {msg}");
    }
}
