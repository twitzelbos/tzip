//! Per-worker cache of open directory file descriptors, so the
//! extract hot path can `openat(dir_fd, filename, …)` instead of
//! `open(absolute_path, …)`.
//!
//! On APFS and most unix filesystems, the kernel still has to do
//! its inode-creation + journal work per file open — but it can
//! skip the `.` / `..` / component-by-component path walk that
//! `open(2)` does from `/`. For a flat extract (every entry lands
//! in one target directory) that's one cached `dir_fd` for the
//! whole extract. For a nested archive we keep a small LRU so
//! recently-used parents stay hot.
//!
//! Safe fallback: if openat fails for any reason (EACCES, EXDEV,
//! anything exotic) we propagate the error — the outer worker loop
//! already handles the `anyhow` wrap. Unknown-path edge cases that
//! `File::create` would handle differently aren't currently in
//! scope (extracts have been through `safe_join` + mkdir upstream).

#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::path::{Path, PathBuf};

/// Default LRU capacity. 16 open dir fds is enough to keep every
/// recently-touched subtree hot for most real archives; worst case
/// is the fd churn of a deep-nested archive where entries alternate
/// parents faster than the LRU window, which just degrades to the
/// baseline `open(absolute_path)` cost.
#[cfg(unix)]
pub const DEFAULT_DIRFD_CAP: usize = 16;

#[cfg(unix)]
pub struct DirFdCache {
    entries: VecDeque<(PathBuf, OwnedFd)>,
    cap: usize,
}

#[cfg(unix)]
impl DirFdCache {
    pub fn new(cap: usize) -> Self {
        Self { entries: VecDeque::with_capacity(cap), cap }
    }

    /// Open `dest` for write (create+truncate) via `openat` against
    /// the cached parent-dir fd. Returns a regular `File` the caller
    /// can write / fstat / fchmod / futimens against exactly like
    /// `File::create`'s result.
    pub fn create_at(&mut self, dest: &Path) -> io::Result<File> {
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        let parent_path = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let filename = dest.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
        })?;
        let dir_fd = self.get_or_open(parent_path)?;

        // CStr conversion: ZIP names are bytes; build directly to
        // avoid a UTF-8 round-trip that might reject valid filenames.
        let mut name_c: Vec<u8> = Vec::with_capacity(filename.as_bytes().len() + 1);
        name_c.extend_from_slice(filename.as_bytes());
        if name_c.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filename contains NUL byte",
            ));
        }
        name_c.push(0);

        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC;
        // Default mode matches `File::create` behaviour (kernel ANDs
        // with umask). `apply_metadata` on the returned fd tightens /
        // relaxes to the archived mode as needed.
        let mode: libc::mode_t = 0o644;
        // SAFETY: dir_fd is an fd we own via the cached `OwnedFd`.
        // `name_c` is a NUL-terminated byte slice. Return value is
        // checked before wrapping into a `File`.
        let fd = unsafe {
            libc::openat(dir_fd, name_c.as_ptr() as *const libc::c_char, flags, mode as libc::c_int)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a freshly-opened fd we own; `File::from_raw_fd`
        // takes ownership and will close it on drop.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn get_or_open(&mut self, parent: &Path) -> io::Result<libc::c_int> {
        if let Some(pos) = self.entries.iter().position(|(p, _)| p == parent) {
            // Move hit to the front (most-recently-used).
            if pos != 0 {
                if let Some(item) = self.entries.remove(pos) {
                    let fd = item.1.as_raw_fd();
                    self.entries.push_front(item);
                    return Ok(fd);
                }
            }
            return Ok(self.entries[0].1.as_raw_fd());
        }
        // Miss: open the directory. Plain `File::open` on a dir
        // works on macOS + Linux; the fd behaves identically for
        // `openat` purposes.
        let f = File::open(parent)?;
        let fd = f.as_raw_fd();
        // SAFETY: `File::into_raw_fd` would also work; we use
        // `OwnedFd::from(File)` to transfer ownership cleanly.
        let owned = OwnedFd::from(f);
        self.entries.push_front((parent.to_path_buf(), owned));
        while self.entries.len() > self.cap {
            self.entries.pop_back();
        }
        Ok(fd)
    }
}

// Windows + other non-unix platforms: no openat, caller falls back
// to `File::create`.
#[cfg(not(unix))]
pub const DEFAULT_DIRFD_CAP: usize = 0;

#[cfg(not(unix))]
pub struct DirFdCache;

#[cfg(not(unix))]
impl DirFdCache {
    pub fn new(_cap: usize) -> Self {
        Self
    }
    pub fn create_at(&mut self, dest: &std::path::Path) -> std::io::Result<std::fs::File> {
        std::fs::File::create(dest)
    }
}
