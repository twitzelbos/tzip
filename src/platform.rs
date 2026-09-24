//! Platform-specific hints that keep tzip friendly on external drives.
//!
//! The core goal: never evict the user's working set from the page cache
//! just because we're streaming through several gigabytes of file data.

use std::fs::{File, OpenOptions};
use std::io;
use std::io::Read;
use std::path::Path;

/// An owned directory file descriptor. Closed via `close()` on drop.
///
/// Shared across `WorkItem`s from the same directory via `Arc<OwnedDirFd>`
/// so many files can share one dirfd — cheap open, no path resolution per
/// file downstream.
#[cfg(unix)]
pub struct OwnedDirFd {
    pub fd: std::os::unix::io::RawFd,
}

#[cfg(unix)]
impl OwnedDirFd {
    /// Open `path` as a directory (`O_RDONLY | O_DIRECTORY | O_CLOEXEC`).
    pub fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    pub fn as_raw(&self) -> std::os::unix::io::RawFd {
        self.fd
    }
}

#[cfg(unix)]
impl Drop for OwnedDirFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// `fcntl(fd, F_RDADVISE, {offset, len})` — tells the kernel to prefetch
/// `len` bytes from `offset` into the page cache. Unused reads still work
/// if the hint fails; treat errors as best-effort.
#[cfg(target_os = "macos")]
pub fn rdadvise(fd: std::os::unix::io::RawFd, offset: i64, len: u64) -> io::Result<()> {
    // ra_count is a C int (i32). Cap at i32::MAX (~2 GiB); larger files still
    // benefit from the initial 2 GiB of prefetch and then flow through the
    // reader normally.
    let capped: libc::c_int = len.min(i32::MAX as u64) as libc::c_int;
    let radv = libc::radvisory { ra_offset: offset, ra_count: capped };
    let rc = unsafe { libc::fcntl(fd, libc::F_RDADVISE, &radv as *const _) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn rdadvise(_fd: std::os::unix::io::RawFd, _offset: i64, _len: u64) -> io::Result<()> {
    Ok(())
}

/// `openat(dirfd, name, O_RDONLY | O_CLOEXEC)`. Bypasses the pathwalk
/// portion of `open()` — meaningful on deep trees where component lookups
/// each pay a metadata round trip.
#[cfg(unix)]
pub fn open_at(dirfd: std::os::unix::io::RawFd, name: &std::ffi::OsStr) -> io::Result<File> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    let c = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name has NUL"))?;
    let fd = unsafe { libc::openat(dirfd, c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// A byte buffer that either owns its bytes or borrows from a memory map.
/// `Deref<Target = [u8]>` means compressors and CRC hashers see a plain
/// `&[u8]` — no extra copy on the hot path.
pub enum ReadBuf {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl std::ops::Deref for ReadBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            ReadBuf::Owned(v) => v.as_slice(),
            ReadBuf::Mapped(m) => &m[..],
        }
    }
}

impl ReadBuf {
    pub fn len(&self) -> usize {
        match self {
            ReadBuf::Owned(v) => v.len(),
            ReadBuf::Mapped(m) => m.len(),
        }
    }
}

/// Files at or above this size get memory-mapped. Below it, we do a plain
/// `read_to_end` — smaller than a page or two, mmap setup costs win.
pub const MMAP_THRESHOLD: u64 = 1 * 1024 * 1024;

/// One-stop read routine used by the pipeline.
///
/// * files ≥ `MMAP_THRESHOLD` → `Mmap` (zero-copy; compressor reads directly
///   from the page cache)
/// * smaller files → `Vec` with `F_NOCACHE` set so the archive pass doesn't
///   evict the user's working set (mmap and `F_NOCACHE` don't compose)
pub fn read_input(path: &Path, size: u64, keep_cache: bool) -> io::Result<ReadBuf> {
    let f = OpenOptions::new().read(true).open(path)?;
    read_from_file(f, size, keep_cache)
}

/// Same as `read_input` but the caller supplies the already-opened `File`
/// (e.g. one that came from `openat(dirfd, name)`).
pub fn read_from_file(mut f: File, size: u64, keep_cache: bool) -> io::Result<ReadBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // Fire-and-forget prefetch hint — kernel starts pulling pages into the
        // page cache while we set up the read/mmap.
        let _ = rdadvise(f.as_raw_fd(), 0, size);
    }
    if size >= MMAP_THRESHOLD {
        let _ = advise_sequential(&f);
        let mmap = unsafe { memmap2::MmapOptions::new().map(&f)? };
        #[cfg(unix)]
        {
            let _ = mmap.advise(memmap2::Advice::Sequential);
        }
        Ok(ReadBuf::Mapped(mmap))
    } else {
        if !keep_cache {
            let _ = advise_bypass_cache(&f);
        }
        let _ = advise_sequential(&f);
        let mut buf = Vec::with_capacity(size as usize);
        f.read_to_end(&mut buf)?;
        Ok(ReadBuf::Owned(buf))
    }
}

#[cfg(target_os = "macos")]
pub fn advise_bypass_cache(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // F_NOCACHE = 48 (see /usr/include/sys/fcntl.h). Turns off caching for
    // this open file description; subsequent reads bypass the unified buffer cache.
    const F_NOCACHE: libc::c_int = 48;
    let fd = file.as_raw_fd();
    // Use libc directly so we don't need a portable fcntl wrapper crate.
    let rc = unsafe { libc::fcntl(fd, F_NOCACHE, 1) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn advise_bypass_cache(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // POSIX_FADV_DONTNEED tells the kernel we won't need these pages again.
    // Applied after read to drop them from the page cache. We apply proactively
    // over the whole file (offset=0, len=0 → to EOF).
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc))
    }
}

#[cfg(not(unix))]
pub fn advise_bypass_cache(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn advise_sequential(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // F_RDAHEAD = 45: enable read-ahead for this fd.
        const F_RDAHEAD: libc::c_int = 45;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::fcntl(fd, F_RDAHEAD, 1) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn advise_sequential(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Lightweight per-path filesystem info used by preallocation, BufWriter
/// tuning, and the auto-tune startup logic.
#[derive(Clone, Debug)]
pub struct FsInfo {
    /// Filesystem type from statfs — "apfs", "exfat", "msdos", "ntfs", "hfs", …
    pub fs_type: String,
    /// Optimal I/O block size the OS suggests for this filesystem (bytes).
    pub iosize: u32,
    /// Fundamental block size (bytes) — often the cluster/allocation unit.
    #[allow(dead_code)]
    pub bsize: u32,
    /// Mount point.
    #[allow(dead_code)]
    pub mount_point: std::path::PathBuf,
}

#[cfg(target_os = "macos")]
pub fn fs_info(path: &Path) -> io::Result<FsInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut sfs) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let fs_type = unsafe { std::ffi::CStr::from_ptr(sfs.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let mount_point = unsafe { std::ffi::CStr::from_ptr(sfs.f_mntonname.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(FsInfo {
        fs_type,
        iosize: sfs.f_iosize.max(0) as u32,
        bsize: sfs.f_bsize as u32,
        mount_point: std::path::PathBuf::from(mount_point),
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn fs_info(path: &Path) -> io::Result<FsInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut sfs) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // Best-effort fs_type on Linux — libc gives us f_type as a magic number,
    // not a name. Just report "unknown" and rely on other signals.
    Ok(FsInfo {
        fs_type: "unknown".into(),
        iosize: sfs.f_bsize as u32,
        bsize: sfs.f_bsize as u32,
        mount_point: std::path::PathBuf::from("/"),
    })
}

#[cfg(not(unix))]
pub fn fs_info(_path: &Path) -> io::Result<FsInfo> {
    Ok(FsInfo {
        fs_type: "unknown".into(),
        iosize: 65536,
        bsize: 4096,
        mount_point: std::path::PathBuf::from("/"),
    })
}

/// Reserve `len` bytes of extents for `file` WITHOUT changing its logical
/// size. Best-effort — errors are swallowed by the caller since a failed
/// preallocation just means writes fall back to normal growth.
///
/// - macOS: `F_PREALLOCATE` — first attempt contiguous (`F_ALLOCATECONTIG`),
///   fall back to any layout (`F_ALLOCATEALL`).
/// - Linux: `fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, len)`.
/// - Others: no-op.
#[cfg(target_os = "macos")]
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // fstore_t struct (from <sys/fcntl.h>):
    //   uint32_t fst_flags;    // F_ALLOCATECONTIG | F_ALLOCATEALL | F_ALLOCATEPERSIST
    //   int32_t  fst_posmode;  // F_PEOFPOSMODE | F_VOLPOSMODE
    //   off_t    fst_offset;
    //   off_t    fst_length;
    //   off_t    fst_bytesalloc;
    #[repr(C)]
    struct FStore {
        fst_flags: u32,
        fst_posmode: i32,
        fst_offset: i64,
        fst_length: i64,
        fst_bytesalloc: i64,
    }
    const F_PREALLOCATE: libc::c_int = 42;
    const F_ALLOCATECONTIG: u32 = 0x0000_0002;
    const F_ALLOCATEALL: u32 = 0x0000_0004;
    const F_PEOFPOSMODE: i32 = 3;

    let fd = file.as_raw_fd();

    // Try contiguous first (best for exFAT / FAT-family — avoids extent bloat).
    let mut fs = FStore {
        fst_flags: F_ALLOCATECONTIG,
        fst_posmode: F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: len as i64,
        fst_bytesalloc: 0,
    };
    let rc = unsafe { libc::fcntl(fd, F_PREALLOCATE, &mut fs as *mut _) };
    if rc == -1 {
        // Fall back to non-contiguous
        fs.fst_flags = F_ALLOCATEALL;
        fs.fst_bytesalloc = 0;
        let rc = unsafe { libc::fcntl(fd, F_PREALLOCATE, &mut fs as *mut _) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let rc = unsafe {
        libc::fallocate(
            fd,
            libc::FALLOC_FL_KEEP_SIZE,
            0,
            len as libc::off_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn preallocate(_file: &File, _len: u64) -> io::Result<()> {
    Ok(())
}
