//! Shared `Options` struct used by the create (`pipeline::run`) and
//! update (`update::run_update`) backends, plus `UserSetFlags`.
//!
//! Its own module below `cli` so that compress / decompress /
//! pipeline / update / sevenz don't have to depend on `cli` just to
//! see the Options shape. Previously these all imported from `cli`,
//! which made `cli` a hub that every encoder came back to —
//! feeding the `module-cycle` lint. Now the data lives here; `cli`
//! `pub use`s it so the public API stays the same.

use std::path::PathBuf;

use crate::method::Method;

/// Tracks which auto-tunable flags came from the command line (vs took the
/// default). Auto-tune only overrides fields marked `false` here.
#[derive(Clone, Copy, Debug, Default)]
pub struct UserSetFlags {
    pub keep_cache: bool,
    pub read_jobs: bool,
    pub dispatch_io: bool,
    pub raw_block: bool,
    #[allow(dead_code)]
    pub io_uring: bool,
}

/// Shared options for both `create` and `u`. `update`/`delete`/`atomic` are
/// set by `UpdateArgs::into_options`; `CreateArgs::into_options` leaves them
/// at their defaults (update=false, delete empty, atomic=false).
#[derive(Clone, Debug)]
pub struct Options {
    pub archive: PathBuf,
    pub paths: Vec<PathBuf>,
    pub method: Method,
    pub level: u32,
    pub cpu_jobs: usize,
    pub read_jobs: usize,
    pub walk_jobs: usize,
    pub password: Option<String>,
    pub sort: bool,
    pub exclude: Vec<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub keep_cache: bool,
    #[allow(dead_code)]
    pub streaming: bool,
    pub solid: bool,
    pub tui: bool,
    pub classic_walk: bool,
    pub dispatch_io: bool,
    pub usb_info: bool,
    pub warn_contention: bool,
    #[cfg(any(
        all(feature = "raw-apfs", target_os = "macos"),
        target_os = "linux"
    ))]
    pub raw_block: bool,
    #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
    pub apfs_device: Option<PathBuf>,
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    pub io_uring: bool,
    pub basename_only: bool,
    pub user_flags: UserSetFlags,
    pub update: bool,
    pub delete: Vec<String>,
    pub atomic: bool,
    /// When set, encode_one computes MD5 of the plaintext alongside
    /// CRC-32 and the writer stashes it in a tzip-native extra field.
    pub write_md5: bool,
    /// Same as `write_md5` for SHA-256.
    pub write_sha256: bool,
    /// When set, pipeline writes a `.tzip/names.v1` entry at the end
    /// of the archive carrying an MPH over all entry names.
    pub index_names: bool,
    /// Globs of entries to tokenize + index with a hashed inverted
    /// term index in `.tzip/terms.v1`. Empty = skip.
    pub index_terms: Vec<String>,
    /// jq-style field paths to accumulate facet counts for. Each
    /// becomes a `.tzip/facets.v1/<leaf>` entry. Only JSON entries
    /// matching `index_terms` glob contribute.
    pub facet: Vec<String>,
    /// Path to a columnar-schema JSON file. When set, the pipeline
    /// extracts the declared fields from every JSON entry matching
    /// `index_terms` and emits a `.tzip/columnar.v1.parquet`
    /// sidecar. `tzip query --sql` queries it via DuckDB.
    pub write_columnar: Option<PathBuf>,
}
