//! tzip as a library. The binary target (`src/main.rs`) consumes this
//! via `use tzip::*;`. The cdylib target also builds from this crate,
//! so every `#[no_mangle] extern "C"` function declared in a module
//! here (notably `mount::fskit_abi`) exports from the resulting
//! `libtzip.dylib` for the Swift FSKit extension to link against at
//! bundle-load time.
//!
//! Module layout — flat at the import level (`crate::compress`,
//! `crate::zipreader` etc. all still work), but organised on disk
//! into topical subdirectories (`src/zip/`, `src/index/`, `src/io/`,
//! `src/cmd/`, `src/platform/`) so `ls src/` stays readable instead
//! of being a wall of 40+ files. The `#[path = "..."]` attributes
//! below bind each module to its file on disk without reshaping the
//! public module tree — zero import churn for callers.

// ── core config + glue ────────────────────────────────────────────────────
pub mod cli;
pub mod method;
pub mod options;
pub mod pipeline;
pub mod sevenz;
#[path = "update/local.rs"]
pub mod update;
#[path = "update/s3.rs"]
pub mod update_s3;

// ── OS / platform (`src/platform/mod.rs` is the base platform module) ─────
pub mod platform;
pub mod sources;
pub mod mount;

// ── ZIP format + codecs (`src/zip/`) ──────────────────────────────────────
#[path = "zip/compress.rs"]
pub mod compress;
#[path = "zip/decompress.rs"]
pub mod decompress;
#[path = "zip/crypto.rs"]
pub mod crypto;
#[path = "zip/dos_time.rs"]
pub mod dos_time;
#[path = "zip/zipcrypto.rs"]
pub mod zipcrypto;
#[path = "zip/zipformat.rs"]
pub mod zipformat;
#[path = "zip/zipreader.rs"]
pub mod zipreader;
#[path = "zip/zipwriter.rs"]
pub mod zipwriter;
#[path = "zip/sentinel.rs"]
pub mod sentinel;

// ── Index features (`src/index/`) ─────────────────────────────────────────
#[path = "index/aggregate.rs"]
pub mod aggregate;
#[path = "index/columnar.rs"]
pub mod columnar;
#[path = "index/facets.rs"]
pub mod facets;
#[path = "index/indexcache.rs"]
pub mod indexcache;
#[path = "index/cdr_cache.rs"]
pub mod cdr_cache;
#[path = "index/manifest.rs"]
pub mod manifest;
#[path = "index/nameidx.rs"]
pub mod nameidx;
#[path = "index/parquet_prune.rs"]
pub mod parquet_prune;
#[path = "index/query.rs"]
pub mod query;
#[path = "index/shards.rs"]
pub mod shards;
#[path = "index/termidx.rs"]
pub mod termidx;

// ── Read-side I/O primitives (`src/io/`) ──────────────────────────────────
#[path = "io/progress.rs"]
pub mod progress;
#[path = "io/source.rs"]
pub mod source;
#[path = "io/tui.rs"]
pub mod tui;
#[path = "io/walker.rs"]
pub mod walker;

// ── Subcommands (`src/cmd/`) ──────────────────────────────────────────────
#[path = "cmd/bench.rs"]
pub mod bench;
#[path = "cmd/diff.rs"]
pub mod diff;
#[path = "cmd/tail.rs"]
pub mod tail;
#[path = "cmd/find.rs"]
pub mod find;
#[path = "cmd/grep.rs"]
pub mod grep;
#[path = "cmd/hash.rs"]
pub mod hash;
#[path = "cmd/jq.rs"]
pub mod jq;
#[path = "cmd/reader.rs"]
pub mod reader;
#[path = "cmd/report.rs"]
pub mod report;

// ── Platform-gated modules (also live under `src/platform/`) ──────────────
#[cfg(target_os = "macos")]
#[path = "io/bulk_walker.rs"]
pub mod bulk_walker;

#[cfg(target_os = "macos")]
#[path = "platform/dispatch_io.rs"]
pub mod dispatch_io;

#[cfg(target_os = "macos")]
#[path = "platform/usb_info.rs"]
pub mod usb_info;

#[cfg(target_os = "macos")]
#[path = "platform/contention.rs"]
pub mod contention;

#[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
#[path = "platform/raw_apfs.rs"]
pub mod raw_apfs;

#[cfg(target_os = "linux")]
#[path = "platform/fiemap.rs"]
pub mod fiemap;

#[cfg(target_os = "linux")]
#[path = "platform/linux_raw.rs"]
pub mod linux_raw;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[path = "platform/io_uring_src.rs"]
pub mod io_uring_src;

#[path = "platform/dirfd_cache.rs"]
pub mod dirfd_cache;
