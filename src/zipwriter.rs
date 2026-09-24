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
use std::io::{Seek, Write};

pub const SIG_LFH: u32 = 0x0403_4b50;
pub const SIG_CDR: u32 = 0x0201_4b50;
pub const SIG_EOCD: u32 = 0x0605_4b50;
pub const SIG_EOCD64: u32 = 0x0606_4b50;
pub const SIG_EOCD64_LOC: u32 = 0x0706_4b50;

pub const METHOD_AES: u16 = 99;
pub const AES_EXTRA_ID: u16 = 0x9901;

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
}

pub struct ZipWriter<W: Write + Seek> {
    inner: W,
    entries: Vec<CentralEntry>,
    pos: u64,
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
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, entries: Vec::new(), pos: 0 }
    }

    pub fn write_entry(&mut self, e: &EncodedEntry) -> Result<()> {
        let name_bytes = e.name.as_bytes();
        let comp_size = e.body.len() as u64;
        let uncomp_size = e.uncompressed_size;
        let needs_zip64 = comp_size >= 0xFFFF_FFFF || uncomp_size >= 0xFFFF_FFFF;

        let lfh_offset = self.pos;

        // -- Local file header
        let mut lfh = Vec::with_capacity(30 + name_bytes.len() + 16);
        lfh.write_u32::<LittleEndian>(SIG_LFH)?;
        let vn = if needs_zip64 { e.version_needed.max(45) } else { e.version_needed.max(20) };
        lfh.write_u16::<LittleEndian>(vn)?; // version needed to extract
        // General purpose bit flag: bit 0 = encrypted (WinZIP AES requires it),
        // bit 1 = "LZMA EOS marker present" for method 14, bit 11 = UTF-8 name.
        let mut gpbf: u16 = 1 << 11;
        if e.encrypted {
            gpbf |= 1;
        }
        if e.gp_bit1 {
            gpbf |= 1 << 1;
        }
        lfh.write_u16::<LittleEndian>(gpbf)?;
        lfh.write_u16::<LittleEndian>(e.method_on_wire)?;
        lfh.write_u16::<LittleEndian>(e.mtime.1)?; // time
        lfh.write_u16::<LittleEndian>(e.mtime.0)?; // date
        // CRC-32: zero on wire for AE-2
        let wire_crc = if e.encrypted { 0 } else { e.crc32 };
        lfh.write_u32::<LittleEndian>(wire_crc)?;
        if needs_zip64 {
            lfh.write_u32::<LittleEndian>(0xFFFF_FFFF)?; // compressed size (in extra)
            lfh.write_u32::<LittleEndian>(0xFFFF_FFFF)?; // uncompressed size (in extra)
        } else {
            lfh.write_u32::<LittleEndian>(comp_size as u32)?;
            lfh.write_u32::<LittleEndian>(uncomp_size as u32)?;
        }
        lfh.write_u16::<LittleEndian>(name_bytes.len() as u16)?;
        // Extra field length = zip64 (0 or 20) + AES (0 or 11)
        let extra_len = (if needs_zip64 { 4 + 16 } else { 0 })
            + (if e.encrypted { 4 + 7 } else { 0 });
        lfh.write_u16::<LittleEndian>(extra_len as u16)?;
        lfh.extend_from_slice(name_bytes);
        if needs_zip64 {
            lfh.write_u16::<LittleEndian>(0x0001)?; // zip64
            lfh.write_u16::<LittleEndian>(16)?;
            lfh.write_u64::<LittleEndian>(uncomp_size)?;
            lfh.write_u64::<LittleEndian>(comp_size)?;
        }
        if e.encrypted {
            lfh.write_u16::<LittleEndian>(AES_EXTRA_ID)?;
            lfh.write_u16::<LittleEndian>(7)?;
            lfh.write_u16::<LittleEndian>(2)?; // AE-2
            lfh.extend_from_slice(b"AE"); // vendor ID
            lfh.write_u8(0x03)?; // 0x01=128, 0x02=192, 0x03=256
            lfh.write_u16::<LittleEndian>(e.inner_method)?;
        }

        self.inner.write_all(&lfh)?;
        self.inner.write_all(&e.body)?;
        self.pos += lfh.len() as u64 + comp_size;

        self.entries.push(CentralEntry {
            name: name_bytes.to_vec(),
            method_on_wire: e.method_on_wire,
            inner_method: e.inner_method,
            encrypted: e.encrypted,
            gp_bit1: e.gp_bit1,
            version_needed: vn,
            crc32: if e.encrypted { 0 } else { e.crc32 },
            comp_size,
            uncomp_size,
            lfh_offset,
            mtime: e.mtime,
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
            cdr.write_u16::<LittleEndian>((z64.len() + aes_len) as u16)?;
            cdr.write_u16::<LittleEndian>(0)?; // file comment length
            cdr.write_u16::<LittleEndian>(0)?; // disk #
            cdr.write_u16::<LittleEndian>(0)?; // internal attr
            // external attr: high 16 bits = Unix st_mode (0o100644 = regular file rw-r--r--)
            cdr.write_u32::<LittleEndian>(0o100644 << 16)?;
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
