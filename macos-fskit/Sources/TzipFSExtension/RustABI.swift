//
// RustABI.swift
//
// Swift view of the extern "C" symbols defined in src/mount/fskit_abi.rs.
// `cbindgen` generates a matching `tzip_fskit_abi.h` at build time; this
// file redeclares the symbols with `@_silgen_name` so we don't need a
// bridging header or clang module map. Keep in sync if the Rust ABI
// changes.
//
// Return codes: 0 = OK, negative = POSIX errno (ENOENT = -2, EIO = -5,
// EINVAL = -22, ENOTDIR = -20, EISDIR = -21). See fskit_abi.rs.
//

import Foundation

/// Opaque handle. Mirrors `TzipVfsHandle { inner: *mut c_void }` from Rust.
public struct TzipVfsHandle {
    var inner: UnsafeMutableRawPointer?
}

/// Mirrors `CAttr` from Rust. Field layout must match — if you edit one
/// side, edit both.
public struct CAttr {
    var ino: UInt64 = 0
    var size: UInt64 = 0
    /// 0 = file, 1 = directory.
    var kind: UInt8 = 0
    var mode: UInt32 = 0
    /// Unix seconds since epoch.
    var mtime_unix: Int64 = 0
}

/// Mirrors `CDirEntry`. `name` is a Rust-allocated C string; call
/// `tzip_free_string` after reading.
public struct CDirEntry {
    var ino: UInt64 = 0
    var kind: UInt8 = 0
    var name: UnsafeMutablePointer<CChar>? = nil
}

@_silgen_name("tzip_open_volume")
func tzip_open_volume(
    _ archive_url: UnsafePointer<CChar>,
    _ password: UnsafePointer<CChar>?,
    _ cache_mb: UInt32,
    _ verbose: UInt8,
    _ out_handle: UnsafeMutablePointer<TzipVfsHandle>,
    _ out_err: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> Int32

@_silgen_name("tzip_close_volume")
func tzip_close_volume(_ handle: TzipVfsHandle)

@_silgen_name("tzip_root_ino")
func tzip_root_ino() -> UInt64

@_silgen_name("tzip_getattr")
func tzip_getattr(
    _ handle: TzipVfsHandle,
    _ ino: UInt64,
    _ out_attr: UnsafeMutablePointer<CAttr>
) -> Int32

@_silgen_name("tzip_lookup")
func tzip_lookup(
    _ handle: TzipVfsHandle,
    _ parent: UInt64,
    _ name: UnsafePointer<UInt8>,
    _ name_len: Int,
    _ out_attr: UnsafeMutablePointer<CAttr>
) -> Int32

@_silgen_name("tzip_readdir")
func tzip_readdir(
    _ handle: TzipVfsHandle,
    _ ino: UInt64,
    _ out_entries: UnsafeMutablePointer<CDirEntry>,
    _ max_entries: Int,
    _ written: UnsafeMutablePointer<Int>
) -> Int32

@_silgen_name("tzip_read")
func tzip_read(
    _ handle: TzipVfsHandle,
    _ ino: UInt64,
    _ offset: UInt64,
    _ size: UInt32,
    _ out_buf: UnsafeMutablePointer<UInt8>,
    _ written: UnsafeMutablePointer<Int>
) -> Int32

@_silgen_name("tzip_free_string")
func tzip_free_string(_ s: UnsafeMutablePointer<CChar>?)
