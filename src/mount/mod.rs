//! `tzip mount` — read-only filesystem view of a ZIP archive.
//!
//! Platform layers:
//! - **Linux**: `fuse3` via the `fuser` crate (shipped in this release).
//! - **macOS**: FSKit on macOS 15+ is a planned follow-up PR requiring a
//!   Swift bridge module. On any non-Linux host, `tzip mount` fails
//!   fast with a clear "planned follow-up" message.
//!
//! The platform-agnostic logic (directory tree built from the CDR,
//! per-entry decompressed-body LRU cache, password-protected AES reads)
//! lives in `vfs.rs` and is driven by the platform adapter.

#[cfg(target_os = "linux")]
pub mod vfs;

#[cfg(target_os = "linux")]
pub mod fuse3;

use anyhow::Result;
use std::path::PathBuf;

/// Options forwarded to the platform mount implementation. On non-Linux
/// builds the fields go unused (the stub just emits a "planned follow-up"
/// error); silence dead-code warnings there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MountOptions {
    /// Archive spec (local path, `http(s)://…` URL, or `s3://bucket/key`).
    pub archive_spec: String,
    pub mountpoint: PathBuf,
    pub cache_mb: usize,
    pub allow_other: bool,
    pub password: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
}

#[cfg(target_os = "linux")]
pub fn mount(opts: MountOptions) -> Result<()> {
    fuse3::mount(opts)
}

#[cfg(not(target_os = "linux"))]
pub fn mount(_opts: MountOptions) -> Result<()> {
    anyhow::bail!(
        "`tzip mount` currently requires Linux (FUSE3). \
         macOS FSKit support is a planned follow-up PR — track the \
         project ROADMAP's Phase G4 continuation entry."
    )
}
