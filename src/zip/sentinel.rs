//! `.tzip/*` sentinel-entry writer.
//!
//! Shared by the create-mode pipeline (`crate::pipeline`) and the
//! update-mode rebuild/append paths (`crate::update`). Lives here
//! — a leaf of the `zip/` layer — so neither of those two higher
//! modules has to call into the other, which would create a
//! module cycle that `deslop-please` rejects in CI.
//!
//! Writes `.tzip/names.v1`, `.tzip/terms.v1`, `.tzip/facets.v1/*`,
//! `.tzip/columnar.v1.parquet` and any of their `.segN` variants.
//! The body is wrapped in a WinZIP AES-256 AE-2 envelope when a
//! password is set on `opts`; ChatGPT review #2 — index payloads
//! are content-derived, so an unencrypted sentinel on an otherwise
//! password-protected archive leaks every entry name, index term,
//! facet value, and columnar row.

use anyhow::Result;
use std::io::{Seek, Write};

use crate::options::Options;
use crate::zipwriter::{EncodedEntry, ZipWriter};

/// Write one `.tzip/*` sentinel entry to `zw`. The payload is
/// stored (uncompressed), with CRC-32 computed over the plaintext
/// body. When `opts.password` is non-empty the body is AE-2
/// encrypted and the entry is marked with method 99 (AES).
pub fn emit<W: Write + Seek>(
    zw: &mut ZipWriter<W>,
    opts: &Options,
    name: &str,
    body: Vec<u8>,
) -> Result<()> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&body);
    let crc = hasher.finalize();
    let body_len = body.len() as u64;
    let (final_body, encrypted, method_on_wire) = match opts.password.as_deref() {
        Some(pw) if !pw.is_empty() => (
            crate::crypto::wrap_ae2(pw, &body)?,
            true,
            crate::zipformat::METHOD_AES,
        ),
        _ => (body, false, 0),
    };
    let e = EncodedEntry {
        index: u64::MAX,
        name: name.to_string(),
        body: final_body,
        uncompressed_size: body_len,
        crc32: crc,
        method_on_wire,
        // `inner_method = 0` (Store) because the sentinel payload
        // isn't compressed before encryption.
        inner_method: 0,
        encrypted,
        gp_bit1: false,
        version_needed: if encrypted { 51 } else { 20 },
        mtime: (0, 0),
        external_attr: 0o100644u32 << 16,
        md5_digest: None,
        sha256_digest: None,
        index_terms: None,
        facet_body: None,
    };
    zw.write_entry(e)
}
