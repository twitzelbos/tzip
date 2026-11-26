//! ETag-keyed central-directory cache (issue #74).
//!
//! The Phase 6 `IndexCache` already caches `.tzip/*` sentinel
//! bodies. What it does *not* cache is the ZIP's own central
//! directory, which on a huge archive dominates the wire cost of
//! every cold `tzip cat` / `tzip x` / `tzip ls`:
//!
//! > 100 K entries → ~7 MB CDR
//! > 1 M entries → ~70 MB CDR
//! > 10 M entries → ~700 MB CDR
//!
//! See `docs/scenarios/s3-huge-archive-single-entry.md` for the
//! derivation. Caching the CDR under the same `(url, ETag)` key
//! the index cache already uses collapses repeat-open cost to
//! a tiny probe + body read regardless of entry count.
//!
//! ## Correctness
//!
//! An S3 object's ETag is a strong fingerprint — any byte change
//! produces a new ETag (new per-entry salts are fresh across
//! uploads, so even an "identical-plaintext" re-upload lands
//! with a different ETag). A cache hit therefore means the
//! CDR bytes are byte-identical to what the server would
//! return on a fresh fetch.
//!
//! Non-S3 HTTP falls back to `Last-Modified` → `Content-Length`
//! via the source's `fingerprint()`; those are what the `.tzip/*`
//! cache already uses. If no fingerprint is available at all
//! `reader::cache_binding_for_remote` currently builds a
//! `len=<N>` weak fingerprint — same trust level as for sentinel
//! entries.
//!
//! ## Blob format
//!
//! Fixed-endian, little-endian throughout.
//!
//! | offset | size | field                                        |
//! |--------|------|----------------------------------------------|
//! | 0      | 4    | magic `"TZCR"`                               |
//! | 4      | 2    | version (currently 1)                        |
//! | 6      | 2    | reserved                                     |
//! | 8      | 8    | file_size                                    |
//! | 16     | 8    | cd_offset                                    |
//! | 24     | 8    | cd_size                                      |
//! | 32     | 8    | entry_count                                  |
//! | 40     | 4    | comment_len (u32; archive-level EOCD comment)|
//! | 44     | …    | comment bytes                                |
//! | 44 + L | cd_size | raw CDR bytes                             |
//!
//! Multi-disk archives are never cached (the `open_source*`
//! paths bail before we'd call `encode`). ZIP64 is handled
//! transparently — the encoded `cd_offset` / `cd_size` /
//! `entry_count` are already the resolved u64 values after the
//! EOCD64 was consulted.

use anyhow::{bail, Context, Result};

use crate::indexcache::{ArchiveId, IndexCache};

/// Reserved entry-name under which the blob is stashed inside
/// the existing Phase 6 cache. Chosen with a leading `.` and no
/// `/` so it can never collide with a real archive entry name
/// (ZIP entries use `/` for directories; our `.tzip/*`
/// sentinels take the `.tzip/` prefix).
pub const CDR_CACHE_KEY: &str = ".cdr-v1";

const MAGIC: &[u8; 4] = b"TZCR";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 44;

#[derive(Debug, Clone)]
pub struct Blob {
    pub file_size: u64,
    pub cd_offset: u64,
    pub cd_size: u64,
    pub entry_count: u64,
    /// Archive-level EOCD comment (empty on most archives).
    /// Stored so re-parse without a remote read still produces
    /// the same `Archive::archive_comment`.
    pub comment: Vec<u8>,
    pub cdr: Vec<u8>,
}

impl Blob {
    pub fn encode(&self) -> Vec<u8> {
        let comment_len = self.comment.len() as u32;
        let mut buf = Vec::with_capacity(HEADER_LEN + self.comment.len() + self.cdr.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
        buf.extend_from_slice(&self.file_size.to_le_bytes());
        buf.extend_from_slice(&self.cd_offset.to_le_bytes());
        buf.extend_from_slice(&self.cd_size.to_le_bytes());
        buf.extend_from_slice(&self.entry_count.to_le_bytes());
        buf.extend_from_slice(&comment_len.to_le_bytes());
        buf.extend_from_slice(&self.comment);
        buf.extend_from_slice(&self.cdr);
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            bail!("CDR cache blob too short: {} bytes", bytes.len());
        }
        if &bytes[0..4] != MAGIC {
            bail!(
                "CDR cache blob has wrong magic (got {:x?}, want TZCR)",
                &bytes[0..4]
            );
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if version != VERSION {
            bail!(
                "CDR cache blob is version {}, this tzip only reads v{}",
                version,
                VERSION
            );
        }
        let file_size = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let cd_offset = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let cd_size = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let entry_count = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let comment_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        if HEADER_LEN + comment_len > bytes.len() {
            bail!(
                "CDR cache blob truncated: declared comment_len {} overruns buffer",
                comment_len
            );
        }
        let comment_end = HEADER_LEN + comment_len;
        let comment = bytes[HEADER_LEN..comment_end].to_vec();
        let cdr_end = comment_end
            .checked_add(cd_size as usize)
            .ok_or_else(|| anyhow::anyhow!("cd_size overflows usize"))?;
        if cdr_end > bytes.len() {
            bail!(
                "CDR cache blob truncated: declared cd_size {} overruns buffer",
                cd_size
            );
        }
        let cdr = bytes[comment_end..cdr_end].to_vec();
        Ok(Self {
            file_size,
            cd_offset,
            cd_size,
            entry_count,
            comment,
            cdr,
        })
    }
}

/// Convenience wrapper around `IndexCache::get` keyed on
/// `CDR_CACHE_KEY`. Returns `None` on absent-or-malformed blob;
/// a malformed blob never aborts the open (we re-fetch instead).
pub fn load(cache: &IndexCache, id: &ArchiveId) -> Option<Blob> {
    let raw = cache.get(id, CDR_CACHE_KEY)?;
    match Blob::decode(&raw) {
        Ok(b) => Some(b),
        Err(_) => None,
    }
}

/// Convenience wrapper around `IndexCache::put`. Soft-fails:
/// a failed cache-write never aborts the open (worst case the
/// next open re-fetches).
pub fn store(cache: &IndexCache, id: &ArchiveId, blob: &Blob) {
    let _ = cache.put(id, CDR_CACHE_KEY, &blob.encode());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty_comment() {
        let b = Blob {
            file_size: 10_000,
            cd_offset: 9_500,
            cd_size: 400,
            entry_count: 4,
            comment: Vec::new(),
            cdr: (0..400u16).map(|i| (i & 0xff) as u8).collect(),
        };
        let bytes = b.encode();
        let back = Blob::decode(&bytes).unwrap();
        assert_eq!(back.file_size, b.file_size);
        assert_eq!(back.cd_offset, b.cd_offset);
        assert_eq!(back.cd_size, b.cd_size);
        assert_eq!(back.entry_count, b.entry_count);
        assert_eq!(back.comment, b.comment);
        assert_eq!(back.cdr, b.cdr);
    }

    #[test]
    fn roundtrip_with_comment() {
        let b = Blob {
            file_size: 1_000_000_000,
            cd_offset: 999_000_000,
            cd_size: 1_000_000,
            entry_count: 10_000,
            comment: b"archive-level comment".to_vec(),
            cdr: vec![0xab; 1_000_000],
        };
        let back = Blob::decode(&b.encode()).unwrap();
        assert_eq!(back.comment, b.comment);
        assert_eq!(back.cdr.len(), 1_000_000);
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = Blob {
            file_size: 0,
            cd_offset: 0,
            cd_size: 0,
            entry_count: 0,
            comment: Vec::new(),
            cdr: Vec::new(),
        }
        .encode();
        bytes[0] = b'X';
        assert!(Blob::decode(&bytes).is_err());
    }

    #[test]
    fn rejects_version_bump() {
        let mut bytes = Blob {
            file_size: 0,
            cd_offset: 0,
            cd_size: 0,
            entry_count: 0,
            comment: Vec::new(),
            cdr: Vec::new(),
        }
        .encode();
        // Overwrite the version u16 at offset 4..6.
        bytes[4] = 99;
        bytes[5] = 0;
        assert!(Blob::decode(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated_cdr() {
        let b = Blob {
            file_size: 10,
            cd_offset: 0,
            cd_size: 100,
            entry_count: 1,
            comment: Vec::new(),
            cdr: vec![0u8; 100],
        };
        let bytes = b.encode();
        // Drop the last 5 bytes of the CDR region.
        let short = &bytes[..bytes.len() - 5];
        assert!(Blob::decode(short).is_err());
    }
}
