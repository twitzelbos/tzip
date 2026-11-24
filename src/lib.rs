//! tzip as a library. The binary target (`src/main.rs`) consumes this
//! via `use tzip::*;`. The cdylib target also builds from this crate,
//! so every `#[no_mangle] extern "C"` function declared in a module
//! here (notably `mount::fskit_abi`) exports from the resulting
//! `libtzip.dylib` for the Swift FSKit extension to link against at
//! bundle-load time.

pub mod bench;
pub mod cli;
pub mod compress;
pub mod crypto;
pub mod decompress;
pub mod diff;
pub mod dos_time;
pub mod find;
pub mod grep;
pub mod hash;
pub mod mount;
pub mod pipeline;
pub mod platform;
pub mod progress;
pub mod reader;
pub mod sevenz;
pub mod sources;
pub mod tui;
pub mod update;
pub mod walker;
pub mod zipcrypto;
pub mod zipreader;
pub mod zipwriter;

#[cfg(target_os = "macos")]
pub mod bulk_walker;

#[cfg(target_os = "macos")]
pub mod dispatch_io;

#[cfg(target_os = "macos")]
pub mod usb_info;

#[cfg(target_os = "macos")]
pub mod contention;

#[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
pub mod raw_apfs;

#[cfg(target_os = "linux")]
pub mod fiemap;

#[cfg(target_os = "linux")]
pub mod linux_raw;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
pub mod io_uring_src;
