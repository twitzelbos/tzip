//! ZIP wire-format constants shared by the reader and writer.
//!
//! Lives in its own module so `zipreader` and `zipwriter` can each
//! depend on constants without depending on each other — that
//! satisfies deslop-please's `module-cycle` lint and, more
//! importantly, lets a reader change (parse a new extra field) ship
//! without rebuilding the writer, and vice-versa. The constants
//! themselves are specified by PKWARE APPNOTE §4 (signatures + AES
//! extra from §4.3, tzip-native hash extra from `docs/wire-format`).

/// Central Directory File Header signature (`PK\x01\x02`).
pub const SIG_CDR: u32 = 0x0201_4b50;
/// Local File Header signature (`PK\x03\x04`).
pub const SIG_LFH: u32 = 0x0403_4b50;
/// End of Central Directory Record signature (`PK\x05\x06`).
pub const SIG_EOCD: u32 = 0x0605_4b50;
/// ZIP64 End of Central Directory Record (`PK\x06\x06`).
pub const SIG_EOCD64: u32 = 0x0606_4b50;
/// ZIP64 EOCD Locator (`PK\x06\x07`).
pub const SIG_EOCD64_LOC: u32 = 0x0706_4b50;

/// Storage method 99 — WinZIP AES-wrapped. See AE-2 spec §4.7.
pub const METHOD_AES: u16 = 99;
/// Extra-field header ID for the WinZIP AES extra (per AE-2 §4.7).
pub const AES_EXTRA_ID: u16 = 0x9901;

/// tzip-native extra field carrying a plaintext digest. Layout:
/// `[algo: u8] [digest: N bytes]`, where algo=1=MD5 (16 bytes) /
/// algo=2=SHA-256 (32 bytes). The 4-byte id+size prefix is standard
/// APPNOTE extra framing. Picked outside the well-known ID range
/// (APPNOTE §4.6) so other tools ignore it cleanly.
pub const TZIP_HASH_EXTRA_ID: u16 = 0x7A68;
pub const TZIP_HASH_ALGO_MD5: u8 = 1;
pub const TZIP_HASH_ALGO_SHA256: u8 = 2;

/// Parsed central-directory entry — the shape both the reader
/// (`Archive`) and the writer (`ZipWriter::carry_existing_entry`,
/// `ZipWriter::record_existing`) agree on. Lives here, not in
/// `zipreader`, so `zipwriter` can consume it without pulling
/// `zipreader` back as a dependency (that was the original
/// `zipreader ↔ zipwriter` module cycle).
#[derive(Clone, Debug)]
pub struct ReadEntry {
    pub name: String,
    pub name_bytes: Vec<u8>,
    pub method_on_wire: u16,
    /// Inner compression method when `encrypted` (from the AES extra).
    /// Equal to `method_on_wire` otherwise.
    pub inner_method: u16,
    pub gp_flags: u16,
    /// "Version made by" — low byte is the ZIP version, high byte is the
    /// originating host OS. Needed so `external_attr` can be interpreted:
    /// `host_os == 3` (Unix) ⇒ high 16 bits of external_attr are Unix mode.
    pub made_by: u16,
    pub version_needed: u16,
    pub crc32: u32,
    pub comp_size: u64,
    pub uncomp_size: u64,
    pub lfh_offset: u64,
    pub mtime: (u16, u16),
    pub encrypted: bool,
    /// External file attributes. On Unix-origin archives
    /// (`host_os(made_by) == 3`), the high 16 bits are the Unix mode
    /// (including type bits + rwx). On DOS-origin archives the low 8
    /// bits carry DOS file attributes. See `unix_mode()`.
    pub external_attr: u32,
    /// Per-entry comment (raw bytes, decoded from CP437 via
    /// `decode_name` semantics — same encoding path as the entry
    /// name).
    pub comment: String,
    /// Raw comment bytes, preserved for round-trip into a rewritten CDR.
    pub comment_bytes: Vec<u8>,
    /// On a single-disk archive this is 0. On a multi-disk archive
    /// this is the disk index the LFH + body live on; `lfh_offset`
    /// is patched by `Archive::open` to be absolute in the
    /// concatenated address space, so downstream readers don't need
    /// to re-apply this.
    pub disk_number_start: u16,
    /// `30 + name_len + extra_len` for this entry's LFH. Zero until
    /// the first call to `Archive::lfh_total_size_of`, which reads
    /// the LFH header bytes to learn `extra_len` (not derivable
    /// from the CDR).
    pub lfh_total_size: u32,
    /// Plaintext MD5 digest lifted from the tzip-native extra field
    /// (id = `TZIP_HASH_EXTRA_ID`, algo = 1). `None` when the
    /// archive wasn't created with `--write-md5`.
    pub md5_digest: Option<[u8; 16]>,
    /// Same for SHA-256 (algo = 2).
    pub sha256_digest: Option<[u8; 32]>,
}

impl ReadEntry {
    /// Returns the Unix mode bits encoded in `external_attr` when
    /// the archive was produced on a Unix host
    /// (`version_made_by` byte 1 == 3). None for DOS-origin,
    /// Windows-origin, Mac-origin archives etc.
    pub fn unix_mode(&self) -> Option<u32> {
        let host_os = (self.made_by >> 8) & 0xff;
        if host_os == 3 {
            Some(self.external_attr >> 16)
        } else {
            None
        }
    }
}
