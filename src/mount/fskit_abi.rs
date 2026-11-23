//! C ABI exported to the Swift FSKit extension (`macos-fskit/`).
//!
//! The Swift side holds an opaque pointer to a boxed `VirtualFs` and
//! translates every FSKit callback into one of the `tzip_*` functions
//! defined here. All strings are UTF-8 NUL-terminated; all return codes
//! are POSIX errno-shaped (`0` = OK, `-ENOENT` etc.).
//!
//! Lifetime rules:
//! - `tzip_open_volume` allocates the `VirtualFs` with `Box::into_raw`;
//!   the returned pointer MUST be released via `tzip_close_volume`.
//! - All other functions borrow the handle non-destructively.
//! - Output buffers (`out_err`, `out_entries`) are caller-allocated;
//!   where we write C strings via `tzip_strdup`, the caller is
//!   responsible for `free()`ing them.
//!
//! cbindgen picks this file up at build time (see build.rs) to emit
//! `target/<profile>/tzip_fskit_abi.h`.

#![allow(clippy::missing_safety_doc)]
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::sync::Arc;

use super::vfs::{Attr, NodeKind, VirtualFs, ROOT_INO};
use crate::reader::open_archive;
use crate::zipreader::Archive;

/// Return codes. 0 = OK. Negative values mirror POSIX errno (e.g. -2 =
/// ENOENT). Unknown/other errors are -EIO (-5).
pub const TZIP_OK: c_int = 0;
pub const TZIP_ENOENT: c_int = -2;
pub const TZIP_EIO: c_int = -5;
pub const TZIP_EINVAL: c_int = -22;
pub const TZIP_ENOTDIR: c_int = -20;
pub const TZIP_EISDIR: c_int = -21;

/// Opaque handle wrapping a boxed `VirtualFs`.
#[repr(C)]
pub struct TzipVfsHandle {
    /// Opaque pointer; treat as `*mut VirtualFs` on the Rust side.
    pub inner: *mut c_void,
}

/// C-friendly per-entry attributes. Matches `vfs::Attr` field-for-field
/// with explicit C layout so Swift can see it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CAttr {
    pub ino: u64,
    pub size: u64,
    /// 0 = file, 1 = directory.
    pub kind: u8,
    /// POSIX mode bits for the item (0o444 or 0o555).
    pub mode: u32,
    /// Unix seconds since epoch.
    pub mtime_unix: i64,
}

/// One directory entry for `tzip_readdir` output.
#[repr(C)]
pub struct CDirEntry {
    pub ino: u64,
    /// 0 = file, 1 = directory.
    pub kind: u8,
    /// NUL-terminated name bytes, allocated with `tzip_strdup`.
    /// Caller MUST free each `name` after consumption.
    pub name: *mut c_char,
}

// ── open / close ──────────────────────────────────────────────────────────

/// Open an archive and construct a `VirtualFs` behind the opaque handle.
/// `archive_url` is a NUL-terminated UTF-8 string (local path, http URL,
/// or s3 URL). `password` may be NULL for non-AES archives.
///
/// On success returns 0 and writes the handle to `*out_handle`.
/// On failure returns a negative errno-shaped code and writes a
/// `tzip_strdup`'d error message to `*out_err` (caller frees).
#[no_mangle]
pub unsafe extern "C" fn tzip_open_volume(
    archive_url: *const c_char,
    password: *const c_char,
    cache_mb: u32,
    verbose: u8,
    out_handle: *mut TzipVfsHandle,
    out_err: *mut *mut c_char,
) -> c_int {
    if archive_url.is_null() || out_handle.is_null() {
        return TZIP_EINVAL;
    }
    let url = match CStr::from_ptr(archive_url).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return write_err(out_err, "archive URL is not valid UTF-8"),
    };
    let pw: Option<String> = if password.is_null() {
        None
    } else {
        match CStr::from_ptr(password).to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => return write_err(out_err, "password is not valid UTF-8"),
        }
    };

    let archive: Archive = match open_archive(&url) {
        Ok(a) => a,
        Err(e) => return write_err(out_err, &format!("open {}: {:#}", url, e)),
    };
    let vfs = match VirtualFs::new(archive, pw, cache_mb.max(1) as usize, verbose != 0) {
        Ok(v) => v,
        Err(e) => return write_err(out_err, &format!("vfs init: {:#}", e)),
    };
    let boxed = Box::new(vfs);
    (*out_handle).inner = Box::into_raw(boxed) as *mut c_void;
    TZIP_OK
}

/// Release a handle previously returned by `tzip_open_volume`.
/// Safe to call with a null handle (no-op).
#[no_mangle]
pub unsafe extern "C" fn tzip_close_volume(handle: TzipVfsHandle) {
    if !handle.inner.is_null() {
        drop(Box::from_raw(handle.inner as *mut VirtualFs));
    }
}

/// Return the root inode (1 by FUSE / FSKit convention). Convenience
/// for the Swift side.
#[no_mangle]
pub unsafe extern "C" fn tzip_root_ino() -> u64 {
    ROOT_INO
}

// ── metadata ──────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn tzip_getattr(
    handle: TzipVfsHandle,
    ino: u64,
    out_attr: *mut CAttr,
) -> c_int {
    if handle.inner.is_null() || out_attr.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    match vfs.getattr(ino) {
        Some(a) => {
            *out_attr = to_cattr(a);
            TZIP_OK
        }
        None => TZIP_ENOENT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn tzip_lookup(
    handle: TzipVfsHandle,
    parent: u64,
    name: *const u8,
    name_len: usize,
    out_attr: *mut CAttr,
) -> c_int {
    if handle.inner.is_null() || name.is_null() || out_attr.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let slice = std::slice::from_raw_parts(name, name_len);
    match vfs.lookup(parent, slice) {
        Some(a) => {
            *out_attr = to_cattr(a);
            TZIP_OK
        }
        None => TZIP_ENOENT,
    }
}

// ── readdir ───────────────────────────────────────────────────────────────

/// Fill `out_entries` (array of `max_entries` `CDirEntry`) with the
/// children of `ino`. Writes the number of entries filled to `*written`.
/// `name` fields are heap-allocated via `tzip_strdup` and MUST be freed
/// by the caller.
///
/// If the directory has more children than `max_entries`, the extra
/// entries are silently dropped. (For MVP, Swift calls with a
/// generously-sized buffer; a cursor/offset API is a follow-up.)
#[no_mangle]
pub unsafe extern "C" fn tzip_readdir(
    handle: TzipVfsHandle,
    ino: u64,
    out_entries: *mut CDirEntry,
    max_entries: usize,
    written: *mut usize,
) -> c_int {
    if handle.inner.is_null() || out_entries.is_null() || written.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let entries = match vfs.readdir(ino) {
        Some(e) => e,
        None => return TZIP_ENOENT,
    };
    let n = entries.len().min(max_entries);
    for (i, (child_ino, kind, name)) in entries.into_iter().take(n).enumerate() {
        let name_cstr = CString::new(name).unwrap_or_else(|_| CString::new("?").unwrap());
        let out = out_entries.add(i);
        (*out).ino = child_ino;
        (*out).kind = kind_to_u8(kind);
        (*out).name = name_cstr.into_raw();
    }
    *written = n;
    TZIP_OK
}

// ── read ──────────────────────────────────────────────────────────────────

/// Read up to `size` bytes at `offset` from file-inode `ino` into
/// `out_buf`. Writes the number of bytes actually copied to `*written`.
#[no_mangle]
pub unsafe extern "C" fn tzip_read(
    handle: TzipVfsHandle,
    ino: u64,
    offset: u64,
    size: u32,
    out_buf: *mut u8,
    written: *mut usize,
) -> c_int {
    if handle.inner.is_null() || out_buf.is_null() || written.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    match vfs.read(ino, offset, size) {
        Ok(bytes) => {
            let n = bytes.len().min(size as usize);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf, n);
            *written = n;
            TZIP_OK
        }
        Err(_) => TZIP_EIO,
    }
}

/// Free a string allocated by the Rust side (used for `CDirEntry.name`
/// and `out_err` of `tzip_open_volume`).
#[no_mangle]
pub unsafe extern "C" fn tzip_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

fn kind_to_u8(k: NodeKind) -> u8 {
    match k {
        NodeKind::File => 0,
        NodeKind::Dir => 1,
    }
}

fn to_cattr(a: Attr) -> CAttr {
    let (kind_u8, mode) = match a.kind {
        NodeKind::File => (0u8, 0o444u32),
        NodeKind::Dir => (1u8, 0o555u32),
    };
    CAttr {
        ino: a.ino,
        size: a.size,
        kind: kind_u8,
        mode,
        mtime_unix: a.mtime_unix,
    }
}

unsafe fn write_err(out_err: *mut *mut c_char, msg: &str) -> c_int {
    if !out_err.is_null() {
        if let Ok(c) = CString::new(msg) {
            *out_err = c.into_raw();
        }
    }
    TZIP_EIO
}

// Keep the `Arc` import useful so cargo doesn't flag it unused on some
// cfg paths; VirtualFs uses Arc internally already.
#[allow(dead_code)]
fn _touch_arc() -> Arc<u8> {
    Arc::new(0)
}
