//! Backing-storage sources for `tzip ls` / `cat` / `x`. The archive parser
//! and per-entry byte range reader both work against a single `ReadSeek`
//! trait, so a local `File`, an HTTP(S) URL, or an S3 object can all be
//! swapped in without changing the parsing or decryption pipeline.
//!
//! - `http` — HTTPS/HTTP source via `ureq` and `Range` headers.
//! - `s3`   — S3 source via hand-rolled SigV4 + `ureq`.
//! - `sigv4` — AWS SigV4 request signer (shared with any future AWS backend).
//!
//! All HTTP(S) / S3 sources share a single process-wide `ureq::Agent`
//! via `shared_agent()`. ureq's Agent is internally reference-counted
//! and owns the keep-alive pool, so cloning it (what `shared_agent()`
//! returns on every call) hands every worker thread the same pool.
//! A multi-worker remote extract or grep now pays the TLS handshake
//! once, not once-per-worker. That's what the Phase 6 design doc
//! meant by "HTTP/2 persistent connection pool so cold-fetches don't
//! pay TLS handshake per request" — ureq is HTTP/1.1, but its
//! keep-alive pool covers the same practical ground when it's
//! actually shared.

use std::sync::OnceLock;
use std::time::Duration;

pub mod http;
pub mod multidisk;
pub mod s3;
pub mod s3_list;
pub mod sigv4;

/// Lazily-initialised process-global HTTP agent. ureq's `Agent` is an
/// `Arc`-wrapped pool handle internally, so `clone()` hands out the
/// same underlying connection pool to every caller — exactly the
/// "share TLS sessions across workers" property we want.
///
/// 60-second timeout matches what every source used to set
/// individually.
pub fn shared_agent() -> ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .user_agent(&format!("tzip/{}", env!("CARGO_PKG_VERSION")))
                .build()
        })
        .clone()
}

/// The combined `Read + Seek + Send` trait our archive sources expose. All
/// three (local file, HTTP range source, S3 range source) implement this.
/// `Send` so the source can be used from worker threads in Phase H1 fan-out.
pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}

impl<T: std::io::Read + std::io::Seek + Send + ?Sized> ReadSeek for T {}
