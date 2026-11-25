//! Compression-method identifier used by both the encoder
//! (`compress`), the decoder (`decompress`), and the pipeline layer.
//!
//! Its own module so those three don't have to depend on `cli` just
//! to see the enum definition. Lives below cli in the dependency
//! graph; cli re-exports it so argv parsing (`clap::ValueEnum`) keeps
//! working without changing user-facing API.

use clap::ValueEnum;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Method {
    Store,
    Deflate,
    Bzip2,
    Lzma,
    Xz,
    Zstd,
    /// PPMd variant Ir1 (ZIP method 98). Context-modelling entropy
    /// coder — great ratio on text, CPU-bound on compression, needs
    /// a few MB to a few hundred MB of model arena per entry (scales
    /// with `--level`). Recipient needs a PPMd-aware unzipper
    /// (7-Zip, WinZip, Info-ZIP builds with PPMd).
    Ppmd,
}
