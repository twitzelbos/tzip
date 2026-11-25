//! Thin, safe-ish wrapper over the Linux `FS_IOC_FIEMAP` ioctl.
//!
//! FIEMAP asks the filesystem where a file's data physically lives on the
//! block device — a list of `(logical, physical, length)` extents — without
//! parsing the on-disk format ourselves. Every major Linux filesystem
//! implements it: ext4, xfs, btrfs, f2fs. This is the Linux analog to the
//! macOS `--raw-block` reader's goal (turn scattered per-file reads into a
//! forward disk sweep), except the kernel hands us the layout via a standard
//! ioctl so we need no vendored filesystem parser.
//!
//! We use it for exactly one thing: the *first physical byte offset* of each
//! file, so the bulk reader can sort files into disk order before reading
//! them through the normal VFS. That converts a random seek storm on
//! spinning / USB media into a monotonic forward sweep.
//!
//! Best-effort throughout: any ioctl failure (unsupported fs, NFS, a file
//! with no data blocks) just means the caller falls back to walker order for
//! that file. FIEMAP is an optimization hint, never a correctness dependency.

#![cfg(target_os = "linux")]

use std::io;
use std::os::unix::io::RawFd;

/// `struct fiemap` header (linux/fiemap.h). The `fm_extents[]` flexible array
/// follows immediately in memory; we allocate a single buffer holding this
/// header plus `count` `FiemapExtent` slots.
#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapHeader {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
}

/// `struct fiemap_extent` (linux/fiemap.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
/// Data location unknown / still delalloc / inline with metadata — for these
/// `fe_physical` is meaningless, so we skip them when picking a sort key.
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;

/// `FS_IOC_FIEMAP = _IOWR('f', 11, struct fiemap)`.
///
/// Encoded with the asm-generic `_IOC` layout, which is correct for every
/// architecture tzip realistically targets (x86, x86_64, arm, aarch64,
/// riscv, s390). The oddball arches (mips, powerpc, sparc, alpha) use a
/// different `_IOC` bit layout; FIEMAP simply won't fire there and the
/// caller falls back to walker order — no misbehavior, just no speedup.
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    const NRSHIFT: u32 = 0;
    const TYPESHIFT: u32 = 8;
    const SIZESHIFT: u32 = 16;
    const DIRSHIFT: u32 = 30;
    ((dir << DIRSHIFT)
        | (ty << TYPESHIFT)
        | (nr << NRSHIFT)
        | (size << SIZESHIFT)) as libc::c_ulong
}

fn fs_ioc_fiemap() -> libc::c_ulong {
    const IOC_WRITE: u32 = 1;
    const IOC_READ: u32 = 2;
    ioc(
        IOC_READ | IOC_WRITE,
        b'f' as u32,
        11,
        std::mem::size_of::<FiemapHeader>() as u32,
    )
}

/// The first physical (on-device) byte offset at which `fd`'s data begins, or
/// `None` if the layout is unavailable (fs doesn't support FIEMAP, empty file,
/// inline/unknown extents, ioctl error).
///
/// Only one extent is requested — that's all the sort key needs, and it keeps
/// the ioctl to a single fast call per file.
pub fn first_physical_offset(fd: RawFd) -> Option<u64> {
    // Header + one extent slot in a single contiguous allocation.
    let hdr_size = std::mem::size_of::<FiemapHeader>();
    let ext_size = std::mem::size_of::<FiemapExtent>();
    let mut buf = vec![0u8; hdr_size + ext_size];

    // SAFETY: buf is large enough for the header followed by one extent.
    let hdr = buf.as_mut_ptr() as *mut FiemapHeader;
    unsafe {
        (*hdr) = FiemapHeader {
            fm_start: 0,
            fm_length: u64::MAX, // FIEMAP_MAX_OFFSET — map from 0 to EOF
            fm_flags: 0,         // no SYNC: don't force writeback just to peek
            fm_mapped_extents: 0,
            fm_extent_count: 1,
            fm_reserved: 0,
        };
    }

    let rc = unsafe { libc::ioctl(fd, fs_ioc_fiemap(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }

    let mapped = unsafe { (*hdr).fm_mapped_extents };
    if mapped == 0 {
        return None;
    }
    // The extent slot sits immediately after the header.
    let ext = unsafe { &*(buf.as_ptr().add(hdr_size) as *const FiemapExtent) };
    let unusable = FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_DATA_INLINE;
    if ext.fe_flags & unusable != 0 {
        return None;
    }
    Some(ext.fe_physical)
}

/// Full extent list for `fd`, in logical order. Not needed for the sort-key
/// fast path but handy for diagnostics (`--verbose` extent dumps) and future
/// per-extent coalescing work. Loops until the `LAST` flag is seen, growing
/// the request in batches.
#[allow(dead_code)]
pub fn extents(fd: RawFd) -> io::Result<Vec<(u64, u64, u64)>> {
    const BATCH: u32 = 64;
    let hdr_size = std::mem::size_of::<FiemapHeader>();
    let ext_size = std::mem::size_of::<FiemapExtent>();
    let mut out = Vec::new();
    let mut next_logical: u64 = 0;

    loop {
        let mut buf = vec![0u8; hdr_size + ext_size * BATCH as usize];
        let hdr = buf.as_mut_ptr() as *mut FiemapHeader;
        unsafe {
            (*hdr) = FiemapHeader {
                fm_start: next_logical,
                fm_length: u64::MAX,
                fm_flags: 0,
                fm_mapped_extents: 0,
                fm_extent_count: BATCH,
                fm_reserved: 0,
            };
        }
        let rc = unsafe { libc::ioctl(fd, fs_ioc_fiemap(), buf.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let mapped = unsafe { (*hdr).fm_mapped_extents } as usize;
        if mapped == 0 {
            break;
        }
        let base = unsafe { buf.as_ptr().add(hdr_size) as *const FiemapExtent };
        let mut last = false;
        for i in 0..mapped {
            let e = unsafe { &*base.add(i) };
            out.push((e.fe_logical, e.fe_physical, e.fe_length));
            if e.fe_flags & FIEMAP_EXTENT_LAST != 0 {
                last = true;
            }
            next_logical = e.fe_logical + e.fe_length;
        }
        if last {
            break;
        }
    }
    Ok(out)
}
