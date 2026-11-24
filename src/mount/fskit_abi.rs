//! C ABI exported to the Swift FSKit extension (`macos-fskit/`).
//!
//! The Swift side holds an opaque pointer to a boxed `VirtualFs` and
//! translates every FSKit callback into one of the `tzip_*` functions
//! defined here. All strings are UTF-8 NUL-terminated; all return codes
//! are POSIX errno-shaped (`0` = OK, `-ENOENT` etc.).
//!
//! Lifetime rules:
//! - `tzip_open_volume` / `tzip_open_volume_rw` allocate the `VirtualFs`
//!   with `Box::into_raw`; the returned pointer MUST be released via
//!   `tzip_close_volume`.
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
use std::path::PathBuf;
use std::sync::Arc;

use super::vfs::{Attr, NodeKind, VirtualFs, ROOT_INO};
use crate::reader::open_archive;
use crate::zipreader::Archive;

/// Return codes. 0 = OK. Negative values mirror POSIX errno (e.g. -2 =
/// ENOENT). Unknown/other errors are -EIO (-5).
pub const TZIP_OK: c_int = 0;
pub const TZIP_ENOENT: c_int = -2;
pub const TZIP_EIO: c_int = -5;
pub const TZIP_EEXIST: c_int = -17;
pub const TZIP_ENOTDIR: c_int = -20;
pub const TZIP_EISDIR: c_int = -21;
pub const TZIP_EINVAL: c_int = -22;
pub const TZIP_ENOTEMPTY: c_int = -39;
pub const TZIP_EROFS: c_int = -30;

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
    /// POSIX mode bits for the item. 0o444/0o555 in a read-only volume;
    /// 0o644/0o755 in a writable one.
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

/// Open an archive and construct a read-only `VirtualFs` behind the
/// opaque handle. `archive_url` is a NUL-terminated UTF-8 string (local
/// path, http URL, or s3 URL). `password` may be NULL for non-AES
/// archives.
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
    open_volume_impl(
        archive_url,
        password,
        cache_mb,
        verbose,
        /*writable=*/ false,
        out_handle,
        out_err,
    )
}

/// Open an archive and construct a **writable** `VirtualFs`. Same
/// contract as `tzip_open_volume` but writes buffer in RAM until
/// `tzip_commit` lands them. Remote archives (http/https/s3) are
/// rejected with -EINVAL + an error string — commit needs atomic
/// rename on a real filesystem path.
#[no_mangle]
pub unsafe extern "C" fn tzip_open_volume_rw(
    archive_url: *const c_char,
    password: *const c_char,
    cache_mb: u32,
    verbose: u8,
    out_handle: *mut TzipVfsHandle,
    out_err: *mut *mut c_char,
) -> c_int {
    open_volume_impl(
        archive_url,
        password,
        cache_mb,
        verbose,
        /*writable=*/ true,
        out_handle,
        out_err,
    )
}

unsafe fn open_volume_impl(
    archive_url: *const c_char,
    password: *const c_char,
    cache_mb: u32,
    verbose: u8,
    writable: bool,
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

    // Writable mounts need atomic rename on a real filesystem path; HTTP
    // + S3 can't support that. Fail loudly instead of silently dropping
    // commits on unmount.
    let is_remote = url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("s3://");
    if writable && is_remote {
        return write_err(
            out_err,
            "writable FSKit mount only supports local archives — \
             HTTP/S3 would have nowhere to commit back to",
        );
    }

    let archive: Archive = match open_archive(&url) {
        Ok(a) => a,
        Err(e) => return write_err(out_err, &format!("open {}: {:#}", url, e)),
    };

    let archive_path = if writable {
        match std::fs::canonicalize(&url) {
            Ok(p) => p,
            Err(e) => {
                return write_err(
                    out_err,
                    &format!("canonicalize archive path {}: {}", url, e),
                );
            }
        }
    } else {
        PathBuf::new()
    };

    let vfs = match VirtualFs::new_full(
        archive,
        pw,
        cache_mb.max(1) as usize,
        verbose != 0,
        writable,
        archive_path,
    ) {
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

/// Whether this handle was opened writable (0 = read-only, 1 = rw).
/// Lets the Swift side decide whether to short-circuit setAttributes /
/// createItem / etc. with EROFS before crossing the ABI.
#[no_mangle]
pub unsafe extern "C" fn tzip_is_writable(handle: TzipVfsHandle) -> u8 {
    if handle.inner.is_null() {
        return 0;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    if vfs.is_writable() {
        1
    } else {
        0
    }
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
            *out_attr = to_cattr(a, vfs.is_writable());
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
            *out_attr = to_cattr(a, vfs.is_writable());
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

// ── write-side (writable mounts only) ─────────────────────────────────────

/// Write `data_len` bytes from `data` at `offset` to file-inode `ino`.
/// On success, writes `data_len` to `*written`. Short writes don't
/// happen — we accept the full buffer or fail.
#[no_mangle]
pub unsafe extern "C" fn tzip_write(
    handle: TzipVfsHandle,
    ino: u64,
    offset: u64,
    data: *const u8,
    data_len: usize,
    written: *mut usize,
) -> c_int {
    if handle.inner.is_null() || data.is_null() || written.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let slice = std::slice::from_raw_parts(data, data_len);
    match vfs.write(ino, offset, slice) {
        Ok(_) => {
            *written = data_len;
            TZIP_OK
        }
        Err(e) => map_err_to_errno(&e),
    }
}

/// Create a new regular file as a child of `parent_ino`. Writes the
/// new inode's attributes to `*out_attr`.
#[no_mangle]
pub unsafe extern "C" fn tzip_create(
    handle: TzipVfsHandle,
    parent_ino: u64,
    name: *const u8,
    name_len: usize,
    out_attr: *mut CAttr,
) -> c_int {
    if handle.inner.is_null() || name.is_null() || out_attr.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let slice = std::slice::from_raw_parts(name, name_len);
    match vfs.create(parent_ino, slice) {
        Ok(a) => {
            *out_attr = to_cattr(a, vfs.is_writable());
            TZIP_OK
        }
        Err(e) => map_err_to_errno(&e),
    }
}

/// Create an empty directory under `parent_ino`.
#[no_mangle]
pub unsafe extern "C" fn tzip_mkdir(
    handle: TzipVfsHandle,
    parent_ino: u64,
    name: *const u8,
    name_len: usize,
    out_attr: *mut CAttr,
) -> c_int {
    if handle.inner.is_null() || name.is_null() || out_attr.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let slice = std::slice::from_raw_parts(name, name_len);
    match vfs.mkdir(parent_ino, slice) {
        Ok(a) => {
            *out_attr = to_cattr(a, vfs.is_writable());
            TZIP_OK
        }
        Err(e) => map_err_to_errno(&e),
    }
}

/// Remove a file entry from `parent_ino`.
#[no_mangle]
pub unsafe extern "C" fn tzip_unlink(
    handle: TzipVfsHandle,
    parent_ino: u64,
    name: *const u8,
    name_len: usize,
) -> c_int {
    if handle.inner.is_null() || name.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let slice = std::slice::from_raw_parts(name, name_len);
    match vfs.unlink(parent_ino, slice) {
        Ok(()) => TZIP_OK,
        Err(e) => map_err_to_errno(&e),
    }
}

/// Remove an empty directory from `parent_ino`.
#[no_mangle]
pub unsafe extern "C" fn tzip_rmdir(
    handle: TzipVfsHandle,
    parent_ino: u64,
    name: *const u8,
    name_len: usize,
) -> c_int {
    if handle.inner.is_null() || name.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let slice = std::slice::from_raw_parts(name, name_len);
    match vfs.rmdir(parent_ino, slice) {
        Ok(()) => TZIP_OK,
        Err(e) => map_err_to_errno(&e),
    }
}

/// Resize file `ino` to exactly `new_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn tzip_truncate(
    handle: TzipVfsHandle,
    ino: u64,
    new_size: u64,
) -> c_int {
    if handle.inner.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    match vfs.truncate(ino, new_size) {
        Ok(()) => TZIP_OK,
        Err(e) => map_err_to_errno(&e),
    }
}

/// Set Unix mode bits on an inode. Persisted into the archive's
/// `external_attr` field on commit.
#[no_mangle]
pub unsafe extern "C" fn tzip_chmod(
    handle: TzipVfsHandle,
    ino: u64,
    mode: u32,
) -> c_int {
    if handle.inner.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    match vfs.chmod(ino, mode) {
        Ok(()) => TZIP_OK,
        Err(e) => map_err_to_errno(&e),
    }
}

/// Set the last-modified timestamp on an inode. Persisted into DOS
/// date/time on commit.
#[no_mangle]
pub unsafe extern "C" fn tzip_set_mtime(
    handle: TzipVfsHandle,
    ino: u64,
    mtime_unix: i64,
) -> c_int {
    if handle.inner.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    match vfs.set_mtime(ino, mtime_unix) {
        Ok(()) => TZIP_OK,
        Err(e) => map_err_to_errno(&e),
    }
}

/// Rename `src_name` under `src_parent_ino` to `dst_name` under
/// `dst_parent_ino`. Returns -EEXIST if the destination exists; files
/// CoW into the dirty journal on rename.
#[no_mangle]
pub unsafe extern "C" fn tzip_rename(
    handle: TzipVfsHandle,
    src_parent_ino: u64,
    src_name: *const u8,
    src_name_len: usize,
    dst_parent_ino: u64,
    dst_name: *const u8,
    dst_name_len: usize,
) -> c_int {
    if handle.inner.is_null() || src_name.is_null() || dst_name.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    let src = std::slice::from_raw_parts(src_name, src_name_len);
    let dst = std::slice::from_raw_parts(dst_name, dst_name_len);
    match vfs.rename(src_parent_ino, src, dst_parent_ino, dst) {
        Ok(()) => TZIP_OK,
        Err(e) => map_err_to_errno(&e),
    }
}

/// Flush pending writes to the on-disk archive via the update engine.
/// Writes the number of new/changed entries to `*dirty_out` and the
/// number of deletions to `*tombstoned_out` on success. Safe to call
/// on a read-only mount (no-op, returns 0).
#[no_mangle]
pub unsafe extern "C" fn tzip_commit(
    handle: TzipVfsHandle,
    dirty_out: *mut usize,
    tombstoned_out: *mut usize,
    out_err: *mut *mut c_char,
) -> c_int {
    if handle.inner.is_null() {
        return TZIP_EINVAL;
    }
    let vfs = &*(handle.inner as *const VirtualFs);
    match vfs.commit() {
        Ok(summary) => {
            if !dirty_out.is_null() {
                *dirty_out = summary.dirty;
            }
            if !tombstoned_out.is_null() {
                *tombstoned_out = summary.tombstoned;
            }
            TZIP_OK
        }
        Err(e) => write_err(out_err, &format!("commit: {:#}", e)),
    }
}

/// Free a string allocated by the Rust side (used for `CDirEntry.name`
/// and `out_err` of `tzip_open_volume` / `tzip_commit`).
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

fn to_cattr(a: Attr, writable: bool) -> CAttr {
    let (kind_u8, mode) = match a.kind {
        NodeKind::File => {
            let m = if writable { 0o644u32 } else { 0o444u32 };
            (0u8, m)
        }
        NodeKind::Dir => {
            let m = if writable { 0o755u32 } else { 0o555u32 };
            (1u8, m)
        }
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

/// Map a `VirtualFs` anyhow::Error to the closest errno-shaped code.
/// The VFS raises error messages that contain `ENOENT` / `EEXIST` /
/// `EROFS` / etc. keywords — we scrape those rather than introducing
/// a typed error enum just for the ABI crossing.
fn map_err_to_errno(err: &anyhow::Error) -> c_int {
    let msg = format!("{err:#}");
    if msg.contains("read-only") {
        TZIP_EROFS
    } else if msg.contains("EEXIST") {
        TZIP_EEXIST
    } else if msg.contains("ENOENT") || msg.contains("EISDIR") || msg.contains("ENOTDIR") {
        TZIP_ENOENT
    } else if msg.contains("ENOTEMPTY") {
        TZIP_ENOTEMPTY
    } else {
        TZIP_EIO
    }
}

// Keep the `Arc` import useful so cargo doesn't flag it unused on some
// cfg paths; VirtualFs uses Arc internally already.
#[allow(dead_code)]
fn _touch_arc() -> Arc<u8> {
    Arc::new(0)
}
