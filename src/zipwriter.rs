//! Streaming ZIP writer.
//!
//! Handles:
//! - Standard local file header (LFH) + file data + central directory (CDR) + EOCD
//! - ZIP64 extra field / EOCD64 / EOCD64 locator when file sizes or archive
//!   exceed the 32-bit limits (or when we don't know sizes ahead of time
//!   in streaming mode)
//! - UTF-8 name flag (bit 11) so filenames round-trip cleanly
//! - WinZIP AES-256 (AE-2): extra field 0x9901, compression method 99
//!
//! References:
//! - PKWARE APPNOTE.TXT v6.3.10
//! - WinZIP AES specification (https://www.winzip.com/en/support/aes-encryption/)

use anyhow::{Context, Result};
use byteorder::{LittleEndian, WriteBytesExt};
use std::io::{Read, Seek, SeekFrom, Write};

use crate::zipreader::ReadEntry;

pub const SIG_LFH: u32 = 0x0403_4b50;
pub const SIG_CDR: u32 = 0x0201_4b50;
pub const SIG_EOCD: u32 = 0x0605_4b50;
pub const SIG_EOCD64: u32 = 0x0606_4b50;
pub const SIG_EOCD64_LOC: u32 = 0x0706_4b50;

pub const METHOD_AES: u16 = 99;
pub const AES_EXTRA_ID: u16 = 0x9901;
/// tzip-native extra field carrying a plaintext digest. Layout:
/// `[algo: u8] [digest: N bytes]`, where algo=1=MD5 (16 bytes) /
/// algo=2=SHA-256 (32 bytes). The 4-byte id+size prefix is standard
/// APPNOTE extra framing. Picked outside the well-known ID range
/// (APPNOTE section 4.6) so other tools ignore it cleanly.
pub const TZIP_HASH_EXTRA_ID: u16 = 0x7A68;
pub const TZIP_HASH_ALGO_MD5: u8 = 1;
pub const TZIP_HASH_ALGO_SHA256: u8 = 2;

/// Per-entry info produced by the workers, consumed by the writer.
pub struct EncodedEntry {
    /// Position in the sorted walk. Used by the writer to re-order arrivals
    /// so `--sort` produces byte-identical archives regardless of worker timing.
    pub index: u64,
    pub name: String,
    /// Final compressed+encrypted body (WinZIP layout if encrypted, plain compressed otherwise)
    pub body: Vec<u8>,
    pub uncompressed_size: u64,
    /// CRC-32 of the original plaintext. Zero on wire when using AE-2.
    pub crc32: u32,
    /// Storage method after AES wrapping (either the raw method id, or 99 if encrypted)
    pub method_on_wire: u16,
    /// Real compression method (0, 8, ...) — needed for the AES extra field
    pub inner_method: u16,
    pub encrypted: bool,
    /// Set GP bit 1 (used by LZMA to signal EOS marker present).
    pub gp_bit1: bool,
    /// Minimum ZIP version required to extract this entry (LZMA=63, DEFLATE=20, ...).
    pub version_needed: u16,
    /// DOS (date, time) tuple
    pub mtime: (u16, u16),
    /// External file attributes. High 16 bits are the Unix mode bits
    /// (type + permission). Zero means "use the writer's default"
    /// (0o100644 for files, 0o040755 for dirs — picked below).
    pub external_attr: u32,
    /// Plaintext MD5 digest, when `--write-md5` was requested. The
    /// writer stashes it in a `TZIP_HASH_EXTRA_ID` extra on both the
    /// LFH and CDR so later `tzip hash --algo md5` reads can skip
    /// body decode entirely.
    pub md5_digest: Option<[u8; 16]>,
    /// Same as `md5_digest` for SHA-256.
    pub sha256_digest: Option<[u8; 32]>,
    /// Unique lowercase ASCII tokens from this entry's plaintext,
    /// collected by the CPU worker when `--index-terms` matched the
    /// entry name. `None` when term-indexing isn't requested or the
    /// name didn't match. The main write loop drains these into the
    /// `TermIndexBuilder` at CDR-emit time so posting-list entry
    /// indices match the final CDR order.
    pub index_terms: Option<Vec<Vec<u8>>>,
    /// Owned copy of the plaintext body, populated by the CPU worker
    /// when the entry name matches `--index-terms` glob AND any
    /// `--facet` path was requested. Workers can't do facet
    /// extraction themselves — each facet is a cross-entry
    /// accumulator — so we ship the bytes to the main thread for
    /// aggregation. `None` otherwise; no overhead when facets are
    /// off.
    pub facet_body: Option<Vec<u8>>,
}

pub struct ZipWriter<W: Write + Seek> {
    inner: W,
    entries: Vec<CentralEntry>,
    pos: u64,
    /// Last position left behind on the source after a
    /// `carry_existing_entry`. When the next carry targets the same
    /// offset (CDR-order rebuild of a gap-free archive — the common
    /// case) we can skip the per-entry lseek.
    carry_src_pos: Option<u64>,
    /// Reusable scratch for assembling each entry's local-file header
    /// before writing. Writer is single-threaded so one buffer is
    /// enough; keeping it avoids an alloc+free per entry.
    lfh_scratch: Vec<u8>,
}

struct CentralEntry {
    name: Vec<u8>,
    method_on_wire: u16,
    inner_method: u16,
    encrypted: bool,
    gp_bit1: bool,
    version_needed: u16,
    crc32: u32,
    comp_size: u64,
    uncomp_size: u64,
    lfh_offset: u64,
    mtime: (u16, u16),
    external_attr: u32,
    md5_digest: Option<[u8; 16]>,
    sha256_digest: Option<[u8; 32]>,
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, entries: Vec::new(), pos: 0, carry_src_pos: None, lfh_scratch: Vec::new() }
    }

    /// Fresh writer positioned at `start_pos`. Used by update:
    /// - rebuild mode: `start_pos = 0` for a fresh temp file.
    /// - append mode: `start_pos = old cd_offset` after truncating the
    ///   existing archive; the writer then appends new LFHs + bodies
    ///   starting where the old CDR used to be.
    /// The caller is responsible for seeking `inner` to `start_pos` before
    /// writing. `finish()` will treat `start_pos` + accumulated writes as
    /// `cd_offset`.
    pub fn new_at(inner: W, start_pos: u64) -> Self {
        Self { inner, entries: Vec::new(), pos: start_pos, carry_src_pos: None, lfh_scratch: Vec::new() }
    }

    /// Record a central-directory entry for a body that is already on disk
    /// at `lfh_offset`. Writes nothing; just pushes the entry so that
    /// `finish()` emits a CDR record covering it. Used by:
    /// - append mode: `lfh_offset` is the old entry's original offset;
    ///   the LFH + body are untouched on disk.
    /// - `carry_existing_entry`: internally, after writing the LFH+body
    ///   at `self.pos`, pass the pre-write offset.
    /// How many entries have been recorded (via `record_existing` or
    /// written via `write_entry`) so far. Used by `tzip u`'s
    /// Phase-4 index-segment writer so a sentinel entry's CDR
    /// position is known at emit time for the name-index MPH.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn record_existing(&mut self, re: &ReadEntry, lfh_offset: u64) {
        self.entries.push(CentralEntry {
            name: re.name_bytes.clone(),
            method_on_wire: re.method_on_wire,
            inner_method: re.inner_method,
            encrypted: re.encrypted,
            gp_bit1: re.gp_flags & (1 << 1) != 0,
            version_needed: re.version_needed.max(20),
            crc32: re.crc32,
            comp_size: re.comp_size,
            uncomp_size: re.uncomp_size,
            lfh_offset,
            mtime: re.mtime,
            // Preserve the on-disk external_attr for byte-copy carries.
            external_attr: re.external_attr,
            // Carry any stored plaintext digests forward unchanged — the
            // byte-copy path doesn't touch the body, so the hashes are
            // still valid for the preserved entry.
            md5_digest: re.md5_digest,
            sha256_digest: re.sha256_digest,
        });
    }

    /// Stream-copy an old entry's LFH + body into the writer at the
    /// current `pos`, then record its central entry at the new offset.
    /// Used by rebuild mode. `src` must be seekable and `re.lfh_total_size`
    /// must be populated (via `Archive::lfh_total_size_of`).
        pub fn carry_existing_entry<R: Read + Seek + ?Sized>(
        &mut self,
        src: &mut R,
        re: &ReadEntry,
    ) -> Result<()> {
        if re.lfh_total_size == 0 {
            anyhow::bail!(
                "carry_existing_entry: lfh_total_size not populated for {}",
                re.name
            );
        }
        let new_offset = self.pos;
        // In CDR order (the common case for a tzip-produced archive
        // with no gaps), consecutive carries land back-to-back on the
        // source and the fd is already at `re.lfh_offset`. Skip the
        // lseek when the previous carry left us exactly there.
        if self.carry_src_pos != Some(re.lfh_offset) {
            src.seek(SeekFrom::Start(re.lfh_offset))
                .with_context(|| format!("seek to LFH of {}", re.name))?;
        }
        let total = re.lfh_total_size as u64 + re.comp_size;
        let mut remaining = total;
        let mut buf = [0u8; 256 * 1024];
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            src.read_exact(&mut buf[..want])
                .with_context(|| format!("read body of {}", re.name))?;
            self.inner
                .write_all(&buf[..want])
                .with_context(|| format!("copy body of {}", re.name))?;
            remaining -= want as u64;
        }
        self.pos += total;
        self.carry_src_pos = Some(re.lfh_offset + total);
        self.record_existing(re, new_offset);
        Ok(())
    }

    pub fn write_entry(&mut self, e: EncodedEntry) -> Result<()> {
        // Consume the entry so we can hand its `name` straight into
        // `CentralEntry` as owned bytes (no clone), and hand its
        // `body` into `write_all` as a drop-at-end move.
        let EncodedEntry {
            index: _,
            name,
            body,
            method_on_wire,
            inner_method,
            encrypted,
            gp_bit1,
            version_needed,
            crc32,
            uncompressed_size: uncomp_size,
            mtime,
            external_attr,
            md5_digest,
            sha256_digest,
            index_terms: _,
            facet_body: _,
        } = e;
        let name_bytes = name.into_bytes();
        let comp_size = body.len() as u64;
        let needs_zip64 = comp_size >= 0xFFFF_FFFF || uncomp_size >= 0xFFFF_FFFF;

        let lfh_offset = self.pos;

        // -- Local file header. Reuse the per-writer scratch by
        // `mem::take`-ing it so we can borrow `self` for `self.inner`
        // during write-out; we swap the (now-filled) buffer back in
        // so its capacity carries across entries. Saves one alloc +
        // one drop per entry on large corpora.
        let mut lfh: Vec<u8> = std::mem::take(&mut self.lfh_scratch);
        lfh.clear();
        lfh.reserve(30 + name_bytes.len() + 32);
        lfh.write_u32::<LittleEndian>(SIG_LFH)?;
        let vn = if needs_zip64 { version_needed.max(45) } else { version_needed.max(20) };
        lfh.write_u16::<LittleEndian>(vn)?; // version needed to extract
        // General purpose bit flag: bit 0 = encrypted (WinZIP AES requires it),
        // bit 1 = "LZMA EOS marker present" for method 14, bit 11 = UTF-8 name.
        let mut gpbf: u16 = 1 << 11;
        if encrypted {
            gpbf |= 1;
        }
        if gp_bit1 {
            gpbf |= 1 << 1;
        }
        lfh.write_u16::<LittleEndian>(gpbf)?;
        lfh.write_u16::<LittleEndian>(method_on_wire)?;
        lfh.write_u16::<LittleEndian>(mtime.1)?; // time
        lfh.write_u16::<LittleEndian>(mtime.0)?; // date
        // CRC-32: zero on wire for AE-2
        let wire_crc = if encrypted { 0 } else { crc32 };
        lfh.write_u32::<LittleEndian>(wire_crc)?;
        if needs_zip64 {
            lfh.write_u32::<LittleEndian>(0xFFFF_FFFF)?; // compressed size (in extra)
            lfh.write_u32::<LittleEndian>(0xFFFF_FFFF)?; // uncompressed size (in extra)
        } else {
            lfh.write_u32::<LittleEndian>(comp_size as u32)?;
            lfh.write_u32::<LittleEndian>(uncomp_size as u32)?;
        }
        lfh.write_u16::<LittleEndian>(name_bytes.len() as u16)?;
        // Extra field length = zip64 (0 or 20) + AES (0 or 11) + tzip-hash (0/21/37/58)
        let hash_md5_len = if e.md5_digest.is_some() { 4 + 1 + 16 } else { 0 };
        let hash_sha_len = if e.sha256_digest.is_some() { 4 + 1 + 32 } else { 0 };
        let extra_len = (if needs_zip64 { 4 + 16 } else { 0 })
            + (if encrypted { 4 + 7 } else { 0 })
            + hash_md5_len
            + hash_sha_len;
        lfh.write_u16::<LittleEndian>(extra_len as u16)?;
        lfh.extend_from_slice(&name_bytes);
        if needs_zip64 {
            lfh.write_u16::<LittleEndian>(0x0001)?; // zip64
            lfh.write_u16::<LittleEndian>(16)?;
            lfh.write_u64::<LittleEndian>(uncomp_size)?;
            lfh.write_u64::<LittleEndian>(comp_size)?;
        }
        if encrypted {
            lfh.write_u16::<LittleEndian>(AES_EXTRA_ID)?;
            lfh.write_u16::<LittleEndian>(7)?;
            lfh.write_u16::<LittleEndian>(2)?; // AE-2
            lfh.extend_from_slice(b"AE"); // vendor ID
            lfh.write_u8(0x03)?; // 0x01=128, 0x02=192, 0x03=256
            lfh.write_u16::<LittleEndian>(inner_method)?;
        }
        if let Some(md5) = &e.md5_digest {
            lfh.write_u16::<LittleEndian>(TZIP_HASH_EXTRA_ID)?;
            lfh.write_u16::<LittleEndian>(1 + 16)?;
            lfh.write_u8(TZIP_HASH_ALGO_MD5)?;
            lfh.extend_from_slice(md5);
        }
        if let Some(sha) = &e.sha256_digest {
            lfh.write_u16::<LittleEndian>(TZIP_HASH_EXTRA_ID)?;
            lfh.write_u16::<LittleEndian>(1 + 32)?;
            lfh.write_u8(TZIP_HASH_ALGO_SHA256)?;
            lfh.extend_from_slice(sha);
        }

        self.inner.write_all(&lfh)?;
        self.inner.write_all(&body)?;
        self.pos += lfh.len() as u64 + comp_size;
        // Put the scratch buffer back so its capacity persists for
        // the next entry.
        self.lfh_scratch = lfh;

        self.entries.push(CentralEntry {
            name: name_bytes,
            method_on_wire,
            inner_method,
            encrypted,
            gp_bit1,
            version_needed: vn,
            crc32: if encrypted { 0 } else { crc32 },
            comp_size,
            uncomp_size,
            lfh_offset,
            mtime,
            external_attr,
            md5_digest,
            sha256_digest,
        });
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        let cd_offset = self.pos;

        for e in &self.entries {
            let mut cdr = Vec::with_capacity(46 + e.name.len() + 32);
            cdr.write_u32::<LittleEndian>(SIG_CDR)?;
            cdr.write_u16::<LittleEndian>(0x031E)?; // version made by: 3.0 (unix), zip 3.0
            cdr.write_u16::<LittleEndian>(e.version_needed.max(45))?; // version needed
            let mut gpbf: u16 = 1 << 11;
            if e.encrypted {
                gpbf |= 1;
            }
            if e.gp_bit1 {
                gpbf |= 1 << 1;
            }
            cdr.write_u16::<LittleEndian>(gpbf)?;
            cdr.write_u16::<LittleEndian>(e.method_on_wire)?;
            cdr.write_u16::<LittleEndian>(e.mtime.1)?;
            cdr.write_u16::<LittleEndian>(e.mtime.0)?;
            cdr.write_u32::<LittleEndian>(e.crc32)?;
            let needs_z64_size = e.comp_size >= 0xFFFF_FFFF || e.uncomp_size >= 0xFFFF_FFFF;
            let needs_z64_offset = e.lfh_offset >= 0xFFFF_FFFF;
            let needs_z64 = needs_z64_size || needs_z64_offset;
            if needs_z64_size {
                cdr.write_u32::<LittleEndian>(0xFFFF_FFFF)?;
                cdr.write_u32::<LittleEndian>(0xFFFF_FFFF)?;
            } else {
                cdr.write_u32::<LittleEndian>(e.comp_size as u32)?;
                cdr.write_u32::<LittleEndian>(e.uncomp_size as u32)?;
            }
            cdr.write_u16::<LittleEndian>(e.name.len() as u16)?;
            // extra field size: aes 11, zip64 variable (0..24)
            let mut z64 = Vec::new();
            if needs_z64 {
                z64.write_u16::<LittleEndian>(0x0001)?;
                let payload_len = (needs_z64_size as u16) * 16 + (needs_z64_offset as u16) * 8;
                z64.write_u16::<LittleEndian>(payload_len)?;
                if needs_z64_size {
                    z64.write_u64::<LittleEndian>(e.uncomp_size)?;
                    z64.write_u64::<LittleEndian>(e.comp_size)?;
                }
                if needs_z64_offset {
                    z64.write_u64::<LittleEndian>(e.lfh_offset)?;
                }
            }
            let aes_len = if e.encrypted { 11 } else { 0 };
            let hash_md5_len = if e.md5_digest.is_some() { 4 + 1 + 16 } else { 0 };
            let hash_sha_len = if e.sha256_digest.is_some() { 4 + 1 + 32 } else { 0 };
            cdr.write_u16::<LittleEndian>((z64.len() + aes_len + hash_md5_len + hash_sha_len) as u16)?;
            cdr.write_u16::<LittleEndian>(0)?; // file comment length
            cdr.write_u16::<LittleEndian>(0)?; // disk #
            cdr.write_u16::<LittleEndian>(0)?; // internal attr
            // external attr: high 16 bits = Unix st_mode. Prefer the
            // entry's own value (preserves source-file modes and chmods
            // from writable mounts); fall back to the legacy default
            // 0o100644 for callers that haven't plumbed the field
            // through yet.
            let ext_attr = if e.external_attr != 0 {
                e.external_attr
            } else {
                0o100644u32 << 16
            };
            cdr.write_u32::<LittleEndian>(ext_attr)?;
            if needs_z64_offset {
                cdr.write_u32::<LittleEndian>(0xFFFF_FFFF)?;
            } else {
                cdr.write_u32::<LittleEndian>(e.lfh_offset as u32)?;
            }
            cdr.extend_from_slice(&e.name);
            cdr.extend_from_slice(&z64);
            if e.encrypted {
                cdr.write_u16::<LittleEndian>(AES_EXTRA_ID)?;
                cdr.write_u16::<LittleEndian>(7)?;
                cdr.write_u16::<LittleEndian>(2)?; // AE-2
                cdr.extend_from_slice(b"AE");
                cdr.write_u8(0x03)?;
                cdr.write_u16::<LittleEndian>(e.inner_method)?;
            }
            if let Some(md5) = &e.md5_digest {
                cdr.write_u16::<LittleEndian>(TZIP_HASH_EXTRA_ID)?;
                cdr.write_u16::<LittleEndian>(1 + 16)?;
                cdr.write_u8(TZIP_HASH_ALGO_MD5)?;
                cdr.extend_from_slice(md5);
            }
            if let Some(sha) = &e.sha256_digest {
                cdr.write_u16::<LittleEndian>(TZIP_HASH_EXTRA_ID)?;
                cdr.write_u16::<LittleEndian>(1 + 32)?;
                cdr.write_u8(TZIP_HASH_ALGO_SHA256)?;
                cdr.extend_from_slice(sha);
            }
            self.inner.write_all(&cdr)?;
            self.pos += cdr.len() as u64;
        }
        let cd_size = self.pos - cd_offset;
        let entry_count = self.entries.len() as u64;
        let needs_zip64_eocd = entry_count >= 0xFFFF
            || cd_offset >= 0xFFFF_FFFF
            || cd_size >= 0xFFFF_FFFF;

        if needs_zip64_eocd {
            let eocd64_offset = self.pos;
            let mut eocd64 = Vec::with_capacity(56);
            eocd64.write_u32::<LittleEndian>(SIG_EOCD64)?;
            eocd64.write_u64::<LittleEndian>(44)?; // size of this record - 12
            eocd64.write_u16::<LittleEndian>(0x031E)?;
            eocd64.write_u16::<LittleEndian>(45)?;
            eocd64.write_u32::<LittleEndian>(0)?; // disk #
            eocd64.write_u32::<LittleEndian>(0)?; // disk # w/ start of CD
            eocd64.write_u64::<LittleEndian>(entry_count)?;
            eocd64.write_u64::<LittleEndian>(entry_count)?;
            eocd64.write_u64::<LittleEndian>(cd_size)?;
            eocd64.write_u64::<LittleEndian>(cd_offset)?;
            self.inner.write_all(&eocd64)?;
            self.pos += eocd64.len() as u64;

            let mut loc = Vec::with_capacity(20);
            loc.write_u32::<LittleEndian>(SIG_EOCD64_LOC)?;
            loc.write_u32::<LittleEndian>(0)?; // disk # of EOCD64
            loc.write_u64::<LittleEndian>(eocd64_offset)?;
            loc.write_u32::<LittleEndian>(1)?; // total disks
            self.inner.write_all(&loc)?;
            self.pos += loc.len() as u64;
        }

        let mut eocd = Vec::with_capacity(22);
        eocd.write_u32::<LittleEndian>(SIG_EOCD)?;
        eocd.write_u16::<LittleEndian>(0)?;
        eocd.write_u16::<LittleEndian>(0)?;
        eocd.write_u16::<LittleEndian>(entry_count.min(0xFFFF) as u16)?;
        eocd.write_u16::<LittleEndian>(entry_count.min(0xFFFF) as u16)?;
        eocd.write_u32::<LittleEndian>(cd_size.min(0xFFFF_FFFF) as u32)?;
        eocd.write_u32::<LittleEndian>(cd_offset.min(0xFFFF_FFFF) as u32)?;
        eocd.write_u16::<LittleEndian>(0)?;
        self.inner.write_all(&eocd)?;
        self.pos += eocd.len() as u64;

        self.inner.flush().context("flush archive")?;
        Ok(self.inner)
    }
}
