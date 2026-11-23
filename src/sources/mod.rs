//! Backing-storage sources for `tzip ls` / `cat` / `x`. The archive parser
//! and per-entry byte range reader both work against a single `ReadSeek`
//! trait, so a local `File`, an HTTP(S) URL, or an S3 object can all be
//! swapped in without changing the parsing or decryption pipeline.
//!
//! - `http` — HTTPS/HTTP source via `ureq` and `Range` headers.
//! - `s3`   — S3 source via hand-rolled SigV4 + `ureq`.
//! - `sigv4` — AWS SigV4 request signer (shared with any future AWS backend).

pub mod http;
pub mod s3;
pub mod sigv4;

/// The combined `Read + Seek + Send` trait our archive sources expose. All
/// three (local file, HTTP range source, S3 range source) implement this.
/// `Send` so the source can be used from worker threads in Phase H1 fan-out.
pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}

impl<T: std::io::Read + std::io::Seek + Send + ?Sized> ReadSeek for T {}
