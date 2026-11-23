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

// VFS is cross-platform now — both the Linux FUSE3 adapter and the macOS
// FSKit adapter (behind the `fskit` feature) call into it.
pub mod vfs;

#[cfg(target_os = "linux")]
pub mod fuse3;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub mod fskit;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub mod fskit_abi;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub mod fskit_install;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub mod keychain;

use anyhow::Result;
use std::path::PathBuf;

/// What `tzip mount` should do. On macOS with `--features fskit`,
/// install/uninstall manage the FSKit system extension; on Linux they
/// aren't applicable.
pub enum MountAction {
    Mount(MountOptions),
    #[cfg(all(target_os = "macos", feature = "fskit"))]
    Install,
    #[cfg(all(target_os = "macos", feature = "fskit"))]
    Uninstall,
}

/// Options forwarded to the platform mount implementation. On stub
/// builds (no fskit, no FUSE) the fields go unused — silence warnings.
#[cfg_attr(
    not(any(target_os = "linux", all(target_os = "macos", feature = "fskit"))),
    allow(dead_code)
)]
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

/// Dispatch for the three MountAction variants.
pub fn dispatch(action: MountAction) -> Result<()> {
    match action {
        MountAction::Mount(opts) => mount(opts),
        #[cfg(all(target_os = "macos", feature = "fskit"))]
        MountAction::Install => fskit::install(),
        #[cfg(all(target_os = "macos", feature = "fskit"))]
        MountAction::Uninstall => fskit::uninstall(),
    }
}

#[cfg(target_os = "linux")]
pub fn mount(opts: MountOptions) -> Result<()> {
    fuse3::mount(opts)
}

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub fn mount(opts: MountOptions) -> Result<()> {
    fskit::mount(opts)
}

#[cfg(not(any(target_os = "linux", all(target_os = "macos", feature = "fskit"))))]
pub fn mount(_opts: MountOptions) -> Result<()> {
    anyhow::bail!(
        "`tzip mount` requires Linux (FUSE3) or macOS 15+ built with \
         `--features fskit`. On macOS without the feature, build with \
         `cargo build --features fskit` to enable it."
    )
}
