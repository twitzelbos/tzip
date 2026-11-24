//! ZIP archive reader — EOCD + central-directory + per-entry local-file-header
//! metadata. Enough to drive `--update` / `--delete`: we need each entry's
//! name, offset, sizes, mtime, flags, and the AES-extra inner method; we do
//! *not* need to decompress or decrypt. Kept entries are passed through
//! downstream as byte-for-byte copies.
//!
//! References:
//! - PKWARE APPNOTE.TXT v6.3.10
//! - WinZIP AES specification (0x9901 extra field, AE-2 variant)
//!
//! Scope:
//! - Streams-in-EOCD scan (last 64 KiB), ZIP64 EOCD locator + record.
//! - CDR iteration: parses ZIP64 extra `0x0001` and AES extra `0x9901`.
//! - Lazy LFH probe to compute `lfh_total_size` (needed for byte-copy ranges).
//! - UTF-8 names (GP bit 11). CP437 fallback for the long tail of legacy
//!   tooling — decoded best-effort; the raw `name_bytes` is preserved verbatim
//!   for round-tripping.

use anyhow::{anyhow, bail, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::sources::ReadSeek;
use crate::zipwriter::{AES_EXTRA_ID, METHOD_AES, SIG_CDR, SIG_EOCD, SIG_EOCD64, SIG_EOCD64_LOC, SIG_LFH};

const EOCD_SCAN_WINDOW: u64 = 64 * 1024;
const EOCD_MIN_SIZE: u64 = 22;

/// A central-directory entry, parsed from an existing archive. Enough to
/// re-emit as a central entry in a rewritten archive, and enough for the
/// writer's byte-copy path to locate the original LFH + body on disk.
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
    /// Per-entry comment (raw bytes, decoded from CP437 via `decode_name`
    /// semantics — same encoding path as the entry name).
    pub comment: String,
    /// Raw comment bytes, preserved for round-trip into a rewritten CDR.
    pub comment_bytes: Vec<u8>,
    /// On a single-disk archive this is 0. On a multi-disk archive this
    /// is the disk index the LFH + body live on; `lfh_offset` is
    /// patched by `Archive::open` to be absolute in the concatenated
    /// address space, so downstream readers don't need to re-apply this.
    pub disk_number_start: u16,
    /// `30 + name_len + extra_len` for this entry's LFH. Zero until the
    /// first call to `Archive::lfh_total_size_of`, which reads the LFH header
    /// bytes to learn `extra_len` (not derivable from the CDR).
    pub lfh_total_size: u32,
}

impl ReadEntry {
    /// Returns the Unix mode bits encoded in `external_attr` when the
    /// archive was produced on a Unix host (version_made_by byte 1 == 3).
    /// None for DOS-origin, Windows-origin, Mac-origin archives etc.
    pub fn unix_mode(&self) -> Option<u32> {
        let host_os = (self.made_by >> 8) & 0xff;
        if host_os == 3 {
            Some(self.external_attr >> 16)
        } else {
            None
        }
    }
}

pub struct Archive {
    source: Box<dyn ReadSeek>,
    entries: Vec<ReadEntry>,
    file_size: u64,
    cd_offset: u64,
    cd_size: u64,
    /// `[cd_offset .. file_size]` — the on-disk tail we restore from in
    /// append-mode crash recovery. Cached length only; bytes are read on
    /// demand via `tail_bytes()`.
    tail_size: u64,
    /// Archive-level comment lifted from the EOCD trailer. Decoded as
    /// CP437 (ZIP's default), which passes modern UTF-8 through unchanged.
    archive_comment: String,
    /// Total number of physical disks in the archive. 1 for every
    /// single-stream archive; `n ≥ 2` for a multi-disk spanned archive
    /// stitched via `MultiDiskSource`. Callers that want to spin up
    /// parallel readers against the same archive can use this instead
    /// of re-probing the EOCD themselves.
    disk_count: usize,
}

impl Archive {
    /// Open a local-file archive. Detects multi-disk (spanned)
    /// archives via the EOCD's `this_disk` field and transparently
    /// stitches sibling `.z01`..`.z(N-1)` disks into one concatenated
    /// source. The caller never needs to know how many disks there are.
    pub fn open(path: &Path) -> Result<Self> {
        // Open once, probe the EOCD, and continue parsing from the
        // same handle on the common single-disk path — no second open,
        // no second tail-scan. Only the multi-disk branch has to drop
        // the probe and rebuild the source as a `MultiDiskSource`.
        let mut file = File::open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let file_size = file
            .seek(SeekFrom::End(0))
            .context("seek to end of archive")?;
        if file_size < EOCD_MIN_SIZE {
            bail!("archive is too small to be a ZIP ({} bytes)", file_size);
        }
        let (eocd_pos, eocd) = find_eocd(&mut file, file_size)
            .with_context(|| format!("find EOCD on {}", path.display()))?;

        if eocd.this_disk == 0 && eocd.cd_disk == 0 {
            return Self::finish_parse(Box::new(file), file_size, eocd_pos, eocd)
                .with_context(|| format!("parse {}", path.display()));
        }

        drop(file);
        let total_disks = (eocd.this_disk as usize) + 1;
        let md = crate::sources::multidisk::MultiDiskSource::open(path, total_disks)
            .with_context(|| format!("open multi-disk archive {}", path.display()))?;
        let disk_starts = md.disk_starts().to_vec();
        Self::open_source_multidisk(Box::new(md), disk_starts)
            .with_context(|| format!("parse multi-disk archive {}", path.display()))
    }

    /// Same as `open` but asks the kernel for read+write on the backing
    /// file and hands back a second (dup'd) `File` for the caller to
    /// write through — lets append-mode update avoid the drop-and-reopen
    /// dance between parsing the CDR and truncating the archive at
    /// `cd_offset`.
    ///
    /// Single-disk local archives only — multi-disk append is unsound
    /// (truncating the last disk at the absolute cd_offset would not
    /// land where the CDR actually begins in the concatenated space)
    /// and we reject it up front. Callers that hit the error should
    /// fall back to `Archive::open` + rebuild-mode.
    pub fn open_rw(path: &Path) -> Result<(Self, File)> {
        use std::fs::OpenOptions;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {} rw", path.display()))?;
        let file_size = file
            .seek(SeekFrom::End(0))
            .context("seek to end of archive")?;
        if file_size < EOCD_MIN_SIZE {
            bail!("archive is too small to be a ZIP ({} bytes)", file_size);
        }
        let (eocd_pos, eocd) = find_eocd(&mut file, file_size)
            .with_context(|| format!("find EOCD on {}", path.display()))?;
        if eocd.this_disk != 0 || eocd.cd_disk != 0 {
            bail!(
                "Archive::open_rw does not support multi-disk archives — \
                 use Archive::open and rebuild mode"
            );
        }
        // `try_clone` dup(2)'s the fd. The two fds share the file
        // description (position is shared), which is fine here: Archive
        // parses via the first fd and we explicitly seek the second fd
        // before using it to write.
        let rw_handle = file
            .try_clone()
            .context("dup archive fd for rw hand-off")?;
        let arch = Self::finish_parse(Box::new(file), file_size, eocd_pos, eocd)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok((arch, rw_handle))
    }

    /// Open an archive against any `Read + Seek + Send` source. HTTP, S3,
    /// in-memory cursors — anything implementing `ReadSeek`. The source
    /// must be single-disk; multi-disk archives come through `open`
    /// above (local-only, since sibling disks live on the filesystem).
    pub fn open_source(mut source: Box<dyn ReadSeek>) -> Result<Self> {
        let file_size = source
            .seek(SeekFrom::End(0))
            .context("seek to end of archive source")?;
        if file_size < EOCD_MIN_SIZE {
            bail!("archive is too small to be a ZIP ({} bytes)", file_size);
        }

        let (eocd_pos, eocd) = find_eocd(&mut *source, file_size).context("find EOCD")?;

        if eocd.this_disk != 0 || eocd.cd_disk != 0 {
            bail!(
                "multi-disk archive (this_disk={}, cd_disk={}) opened through a \
                 single-stream source — use Archive::open(path) with the last disk's \
                 local file path so sibling disks can be stitched",
                eocd.this_disk,
                eocd.cd_disk,
            );
        }

        Self::finish_parse(source, file_size, eocd_pos, eocd)
    }

    /// Shared tail of `open` and `open_source`: given a source whose EOCD
    /// has already been located (same seek position on the probe caller),
    /// read the CDR, parse every entry, and assemble the `Archive`. Keeps
    /// the probe out of `open`'s hot path — the local single-disk case
    /// now opens the file exactly once. Only used for single-disk; the
    /// multi-disk branch flows through `open_source_multidisk`.
    fn finish_parse(
        mut source: Box<dyn ReadSeek>,
        file_size: u64,
        eocd_pos: u64,
        eocd: Eocd,
    ) -> Result<Self> {
        let (entry_count, cd_size, cd_offset) = if eocd.needs_zip64() {
            let z64 = find_eocd64(&mut *source, eocd_pos)
                .context("archive looks like ZIP64 but no EOCD64 could be found")?;
            (z64.entry_count, z64.cd_size, z64.cd_offset)
        } else {
            (eocd.entry_count as u64, eocd.cd_size as u64, eocd.cd_offset as u64)
        };

        if cd_offset + cd_size > file_size {
            bail!(
                "central directory extends past end of archive: offset={} size={} file={}",
                cd_offset, cd_size, file_size
            );
        }

        source
            .seek(SeekFrom::Start(cd_offset))
            .with_context(|| format!("seek to central directory at {}", cd_offset))?;
        // Pre-sized Vec + `read_to_end` through a `Take` — fills the
        // buffer in one shot without the zero-memset that `vec![0u8; N]`
        // would pay on top. On a multi-MB CDR the memset was pure waste.
        let mut cdr_bytes: Vec<u8> = Vec::with_capacity(cd_size as usize);
        (&mut *source)
            .take(cd_size)
            .read_to_end(&mut cdr_bytes)
            .context("read central directory")?;
        if cdr_bytes.len() != cd_size as usize {
            bail!("short CDR read: got {} wanted {}", cdr_bytes.len(), cd_size);
        }

        let entries = parse_central_directory(&cdr_bytes, entry_count)?;

        let tail_size = file_size - cd_offset;
        let archive_comment = decode_name(&eocd.comment_bytes, 0);
        Ok(Self {
            source,
            entries,
            file_size,
            cd_offset,
            cd_size,
            tail_size,
            archive_comment,
            disk_count: 1,
        })
    }

    /// Open a multi-disk archive against the concatenated virtual source
    /// built by `MultiDiskSource`. `disk_starts[d]` is the absolute
    /// offset (in the concatenated stream) where disk `d` begins.
    /// Each CDR entry's `(disk_number_start, lfh_offset)` pair is
    /// transformed to an absolute `lfh_offset`; the EOCD's own
    /// `cd_disk`/`cd_offset` pair likewise becomes an absolute CDR
    /// position.
    fn open_source_multidisk(
        mut source: Box<dyn ReadSeek>,
        disk_starts: Vec<u64>,
    ) -> Result<Self> {
        let file_size = source
            .seek(SeekFrom::End(0))
            .context("seek to end of concatenated multi-disk source")?;
        if file_size < EOCD_MIN_SIZE {
            bail!("archive is too small to be a ZIP ({} bytes)", file_size);
        }
        let (_eocd_pos, eocd) =
            find_eocd(&mut *source, file_size).context("find EOCD across stitched disks")?;

        // Multi-disk ZIP64 is extremely rare and would need its own
        // EOCD64-locator search across the probe disk. Out of scope.
        if eocd.needs_zip64() {
            bail!(
                "multi-disk ZIP64 archives are not supported (entry_count={} cd_size={} cd_offset={:#x})",
                eocd.entry_count,
                eocd.cd_size,
                eocd.cd_offset
            );
        }

        let cd_disk_idx = eocd.cd_disk as usize;
        if cd_disk_idx >= disk_starts.len() {
            bail!(
                "EOCD says CDR starts on disk {} but archive has {} disks",
                eocd.cd_disk,
                disk_starts.len()
            );
        }
        let cd_offset = disk_starts[cd_disk_idx]
            .checked_add(eocd.cd_offset as u64)
            .ok_or_else(|| anyhow!("CDR absolute offset overflow"))?;
        let cd_size = eocd.cd_size as u64;
        if cd_offset + cd_size > file_size {
            bail!(
                "central directory extends past end of multi-disk archive: \
                 abs_offset={} size={} total={}",
                cd_offset,
                cd_size,
                file_size
            );
        }

        source
            .seek(SeekFrom::Start(cd_offset))
            .with_context(|| format!("seek to multi-disk CDR at absolute offset {}", cd_offset))?;
        let mut cdr_bytes: Vec<u8> = Vec::with_capacity(cd_size as usize);
        (&mut *source)
            .take(cd_size)
            .read_to_end(&mut cdr_bytes)
            .context("read central directory across stitched disks")?;
        if cdr_bytes.len() != cd_size as usize {
            bail!("short multi-disk CDR read: got {} wanted {}", cdr_bytes.len(), cd_size);
        }
        let mut entries = parse_central_directory(&cdr_bytes, eocd.entry_count as u64)?;

        // Transform (disk, offset) pairs → absolute offsets.
        for entry in &mut entries {
            let d = entry.disk_number_start as usize;
            if d >= disk_starts.len() {
                bail!(
                    "entry {:?} references disk {} but archive has {} disks",
                    entry.name,
                    entry.disk_number_start,
                    disk_starts.len()
                );
            }
            entry.lfh_offset = disk_starts[d]
                .checked_add(entry.lfh_offset)
                .ok_or_else(|| anyhow!("LFH absolute offset overflow for {:?}", entry.name))?;
        }

        let tail_size = file_size - cd_offset;
        let archive_comment = decode_name(&eocd.comment_bytes, 0);
        let disk_count = disk_starts.len();
        Ok(Self {
            source,
            entries,
            file_size,
            cd_offset,
            cd_size,
            tail_size,
            archive_comment,
            disk_count,
        })
    }

    /// Number of physical disks this archive is spanned across. 1 for a
    /// standard single-file ZIP; `n ≥ 2` only for multi-disk archives
    /// opened through `Archive::open`. Lets extract/grep skip a
    /// redundant EOCD probe when fanning out worker sources.
    pub fn disk_count(&self) -> usize {
        self.disk_count
    }

    pub fn entries(&self) -> &[ReadEntry] {
        &self.entries
    }

    /// Archive-level comment from the EOCD trailer. Empty string if the
    /// archive has no comment.
    pub fn archive_comment(&self) -> &str {
        &self.archive_comment
    }

    /// Mutable access to the underlying source so callers can seek + read
    /// specific byte ranges (e.g. to stream an entry's LFH + body).
    pub fn source_mut(&mut self) -> &mut (dyn ReadSeek) {
        &mut *self.source
    }

    pub fn cd_offset(&self) -> u64 {
        self.cd_offset
    }

    #[allow(dead_code)]
    pub fn cd_size(&self) -> u64 {
        self.cd_size
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    #[allow(dead_code)]
    pub fn tail_size(&self) -> u64 {
        self.tail_size
    }

    /// Reads `[cd_offset .. file_size]` — the central directory, EOCD, and
    /// any ZIP64 trailer records. Used to populate the append-mode undo
    /// file so a crashed update can be rolled back.
    pub fn tail_bytes(&mut self) -> Result<Vec<u8>> {
        self.source
            .seek(SeekFrom::Start(self.cd_offset))
            .context("seek to cd_offset for tail copy")?;
        let mut buf: Vec<u8> = Vec::with_capacity(self.tail_size as usize);
        (&mut *self.source)
            .take(self.tail_size)
            .read_to_end(&mut buf)
            .context("read tail bytes")?;
        if buf.len() != self.tail_size as usize {
            bail!("short tail read: got {} wanted {}", buf.len(), self.tail_size);
        }
        Ok(buf)
    }

    /// Lazily populates `entries[i].lfh_total_size` by reading that entry's
    /// LFH header fields. Needed for byte-copy in rebuild mode. Idempotent.
    pub fn lfh_total_size_of(&mut self, idx: usize) -> Result<u32> {
        let cached = self.entries[idx].lfh_total_size;
        if cached != 0 {
            return Ok(cached);
        }
        let off = self.entries[idx].lfh_offset;
        self.source
            .seek(SeekFrom::Start(off))
            .context("seek to LFH")?;
        let mut hdr = [0u8; 30];
        self.source.read_exact(&mut hdr).context("read LFH header")?;
        let sig = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if sig != SIG_LFH {
            bail!("bad LFH signature at offset {}: {:#x}", off, sig);
        }
        let name_len = u16::from_le_bytes([hdr[26], hdr[27]]) as u32;
        let extra_len = u16::from_le_bytes([hdr[28], hdr[29]]) as u32;
        let total = 30 + name_len + extra_len;
        self.entries[idx].lfh_total_size = total;
        Ok(total)
    }
}

/// Cheap EOCD probe: given a seekable source and its total size,
/// returns how many disks this archive spans (1 for single-disk).
/// Used by `reader::open_source_for` so parallel-extract workers can
/// open their own `MultiDiskSource` instances without re-parsing the
/// whole CDR. Errors on malformed archives.
pub fn probe_total_disks<S: Read + Seek + ?Sized>(file: &mut S, file_size: u64) -> Result<usize> {
    if file_size < EOCD_MIN_SIZE {
        bail!("archive is too small to be a ZIP ({} bytes)", file_size);
    }
    let (_, eocd) = find_eocd(file, file_size)?;
    Ok((eocd.this_disk as usize) + 1)
}

/// Parse CDR entries directly from an in-memory archive tail
/// (bytes starting at `cd_offset` — the CDR, then the EOCD, plus any
/// optional ZIP64 trailer). Used by append-mode update's undo
/// snapshot, which has the tail in hand already and shouldn't have
/// to materialise a scratch file on disk, open it, re-scan the EOCD,
/// and re-parse the CDR just to reconstruct the entry list.
///
/// Fails on multi-disk or ZIP64 tails — those paths flow through
/// the rebuild branch of update, not append.
pub fn parse_entries_from_tail(tail: &[u8]) -> Result<Vec<ReadEntry>> {
    use std::io::Cursor;
    let tail_len = tail.len() as u64;
    if tail_len < EOCD_MIN_SIZE {
        bail!("archive tail too small to be a ZIP ({} bytes)", tail_len);
    }
    let mut cur = Cursor::new(tail);
    let (_eocd_pos, eocd) = find_eocd(&mut cur, tail_len).context("find EOCD in tail")?;
    if eocd.needs_zip64() {
        bail!("ZIP64 tail parse not supported via parse_entries_from_tail");
    }
    if eocd.this_disk != 0 || eocd.cd_disk != 0 {
        bail!("multi-disk tail parse not supported via parse_entries_from_tail");
    }
    // Tail starts at cd_offset in the real archive, so the CDR lives
    // at [0 .. cd_size] within `tail`.
    let cd_size = eocd.cd_size as usize;
    if cd_size > tail.len() {
        bail!("tail shorter than declared CDR ({} < {})", tail.len(), cd_size);
    }
    parse_central_directory(&tail[..cd_size], eocd.entry_count as u64)
}

// ── EOCD ────────────────────────────────────────────────────────────────────

struct Eocd {
    this_disk: u16,
    cd_disk: u16,
    entry_count: u16,
    cd_size: u32,
    cd_offset: u32,
    comment_bytes: Vec<u8>,
}

impl Eocd {
    fn needs_zip64(&self) -> bool {
        self.entry_count == 0xFFFF || self.cd_size == 0xFFFF_FFFF || self.cd_offset == 0xFFFF_FFFF
    }
}

fn find_eocd<S: Read + Seek + ?Sized>(file: &mut S, file_size: u64) -> Result<(u64, Eocd)> {
    // Scan the last 64 KiB for the EOCD signature. The EOCD is a 22-byte
    // fixed-size record *followed* by a variable-length comment (0..65535
    // bytes). Scanning from the end lets us find it without knowing the
    // comment length.
    let window = file_size.min(EOCD_SCAN_WINDOW + EOCD_MIN_SIZE);
    let start = file_size - window;
    file.seek(SeekFrom::Start(start))
        .context("seek to EOCD scan window")?;
    let mut buf = vec![0u8; window as usize];
    file.read_exact(&mut buf).context("read EOCD scan window")?;

    let sig = SIG_EOCD.to_le_bytes();
    let mut pos: Option<usize> = None;
    // Prefer the last match (in case of any false-positive earlier signature
    // in a comment). EOCD must leave at least 22 bytes till end of buffer.
    if buf.len() >= 22 {
        let max_i = buf.len() - 22;
        for i in (0..=max_i).rev() {
            if buf[i] == sig[0] && buf[i + 1] == sig[1] && buf[i + 2] == sig[2] && buf[i + 3] == sig[3] {
                // Verify by reading comment length and ensuring it fits.
                let comment_len = u16::from_le_bytes([buf[i + 20], buf[i + 21]]) as usize;
                if i + 22 + comment_len == buf.len() {
                    pos = Some(i);
                    break;
                }
            }
        }
    }
    let i = pos.ok_or_else(|| anyhow!("EOCD signature not found in last {} bytes", window))?;
    let comment_len = u16::from_le_bytes([buf[i + 20], buf[i + 21]]) as usize;
    let comment_start = i + 22;
    let comment_bytes = buf[comment_start..comment_start + comment_len].to_vec();
    let eocd = Eocd {
        this_disk: u16::from_le_bytes([buf[i + 4], buf[i + 5]]),
        cd_disk: u16::from_le_bytes([buf[i + 6], buf[i + 7]]),
        entry_count: u16::from_le_bytes([buf[i + 10], buf[i + 11]]),
        cd_size: u32::from_le_bytes([buf[i + 12], buf[i + 13], buf[i + 14], buf[i + 15]]),
        cd_offset: u32::from_le_bytes([buf[i + 16], buf[i + 17], buf[i + 18], buf[i + 19]]),
        comment_bytes,
    };
    Ok((start + i as u64, eocd))
}

struct Eocd64 {
    entry_count: u64,
    cd_size: u64,
    cd_offset: u64,
}

fn find_eocd64<S: Read + Seek + ?Sized>(file: &mut S, eocd_pos: u64) -> Result<Eocd64> {
    // The EOCD64 locator is exactly 20 bytes immediately before the EOCD.
    if eocd_pos < 20 {
        bail!("no room for EOCD64 locator before EOCD at offset {}", eocd_pos);
    }
    let loc_pos = eocd_pos - 20;
    file.seek(SeekFrom::Start(loc_pos))
        .context("seek to EOCD64 locator")?;
    let mut loc = [0u8; 20];
    file.read_exact(&mut loc).context("read EOCD64 locator")?;
    let sig = u32::from_le_bytes([loc[0], loc[1], loc[2], loc[3]]);
    if sig != SIG_EOCD64_LOC {
        bail!("bad EOCD64 locator signature: {:#x}", sig);
    }
    let z64_off = u64::from_le_bytes(loc[8..16].try_into().unwrap());

    file.seek(SeekFrom::Start(z64_off))
        .context("seek to EOCD64")?;
    let mut hdr = [0u8; 56];
    file.read_exact(&mut hdr).context("read EOCD64")?;
    let z64_sig = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if z64_sig != SIG_EOCD64 {
        bail!("bad EOCD64 signature at {}: {:#x}", z64_off, z64_sig);
    }
    Ok(Eocd64 {
        entry_count: u64::from_le_bytes(hdr[32..40].try_into().unwrap()),
        cd_size: u64::from_le_bytes(hdr[40..48].try_into().unwrap()),
        cd_offset: u64::from_le_bytes(hdr[48..56].try_into().unwrap()),
    })
}

// ── Central directory ──────────────────────────────────────────────────────

fn parse_central_directory(bytes: &[u8], expected_count: u64) -> Result<Vec<ReadEntry>> {
    let mut entries = Vec::with_capacity(expected_count.min(1 << 20) as usize);
    let mut cursor = std::io::Cursor::new(bytes);

    while (cursor.position() as usize) < bytes.len() {
        let sig = cursor.read_u32::<LittleEndian>().context("read CDR sig")?;
        if sig != SIG_CDR {
            // Treat an unexpected signature as end-of-CDR — some tools pad.
            break;
        }
        let made_by = cursor.read_u16::<LittleEndian>()?;
        let version_needed = cursor.read_u16::<LittleEndian>()?;
        let gp_flags = cursor.read_u16::<LittleEndian>()?;
        let method_on_wire = cursor.read_u16::<LittleEndian>()?;
        let time = cursor.read_u16::<LittleEndian>()?;
        let date = cursor.read_u16::<LittleEndian>()?;
        let crc32 = cursor.read_u32::<LittleEndian>()?;
        let comp_size_32 = cursor.read_u32::<LittleEndian>()?;
        let uncomp_size_32 = cursor.read_u32::<LittleEndian>()?;
        let name_len = cursor.read_u16::<LittleEndian>()? as usize;
        let extra_len = cursor.read_u16::<LittleEndian>()? as usize;
        let comment_len = cursor.read_u16::<LittleEndian>()? as usize;
        let disk_start = cursor.read_u16::<LittleEndian>()?;
        let _internal_attr = cursor.read_u16::<LittleEndian>()?;
        let external_attr = cursor.read_u32::<LittleEndian>()?;
        let lfh_offset_32 = cursor.read_u32::<LittleEndian>()?;

        let pos = cursor.position() as usize;
        if pos + name_len + extra_len + comment_len > bytes.len() {
            bail!("central directory entry overflows: pos={} total={}", pos, bytes.len());
        }
        let name_bytes = bytes[pos..pos + name_len].to_vec();
        let extra = &bytes[pos + name_len..pos + name_len + extra_len];
        let comment_bytes =
            bytes[pos + name_len + extra_len..pos + name_len + extra_len + comment_len].to_vec();
        // Skip past name + extra + comment for the next iteration.
        cursor.set_position((pos + name_len + extra_len + comment_len) as u64);

        let mut comp_size: u64 = comp_size_32 as u64;
        let mut uncomp_size: u64 = uncomp_size_32 as u64;
        let mut lfh_offset: u64 = lfh_offset_32 as u64;
        let mut inner_method = method_on_wire;

        // Walk extras: look for ZIP64 (0x0001) and AES (0x9901). Unknown
        // extras are skipped (we only need to round-trip to re-emit).
        let mut ex = 0usize;
        while ex + 4 <= extra.len() {
            let id = u16::from_le_bytes([extra[ex], extra[ex + 1]]);
            let sz = u16::from_le_bytes([extra[ex + 2], extra[ex + 3]]) as usize;
            let body_start = ex + 4;
            if body_start + sz > extra.len() {
                break;
            }
            let body = &extra[body_start..body_start + sz];
            if id == 0x0001 {
                // ZIP64 extended info: fields present only for those whose
                // 32-bit field is 0xFFFFFFFF, in this order:
                //   uncomp_size (8), comp_size (8), lfh_offset (8), disk# (4)
                let mut b = 0;
                if uncomp_size_32 == 0xFFFF_FFFF && b + 8 <= body.len() {
                    uncomp_size = u64::from_le_bytes(body[b..b + 8].try_into().unwrap());
                    b += 8;
                }
                if comp_size_32 == 0xFFFF_FFFF && b + 8 <= body.len() {
                    comp_size = u64::from_le_bytes(body[b..b + 8].try_into().unwrap());
                    b += 8;
                }
                if lfh_offset_32 == 0xFFFF_FFFF && b + 8 <= body.len() {
                    lfh_offset = u64::from_le_bytes(body[b..b + 8].try_into().unwrap());
                }
            } else if id == AES_EXTRA_ID {
                // WinZIP AES 0x9901: [ ver_u16 | 'A' 'E' | strength_u8 | inner_method_u16 ]
                if sz >= 7 {
                    inner_method = u16::from_le_bytes([body[5], body[6]]);
                }
            }
            ex = body_start + sz;
        }

        let encrypted = method_on_wire == METHOD_AES || (gp_flags & 0x0001 != 0);
        let name = decode_name(&name_bytes, gp_flags);
        let comment = decode_name(&comment_bytes, gp_flags);

        entries.push(ReadEntry {
            name,
            name_bytes,
            method_on_wire,
            inner_method: if encrypted { inner_method } else { method_on_wire },
            gp_flags,
            made_by,
            version_needed,
            crc32,
            comp_size,
            uncomp_size,
            lfh_offset,
            mtime: (date, time),
            encrypted,
            external_attr,
            comment,
            comment_bytes,
            disk_number_start: disk_start,
            lfh_total_size: 0,
        });
    }

    Ok(entries)
}

/// Decode a ZIP entry name. Bit 11 of GP flags signals UTF-8 (the way tzip
/// emits them). Otherwise we try UTF-8 and fall back to a lossy CP437-style
/// read of the bytes — the raw `name_bytes` is preserved separately so
/// round-trip into a rewritten CDR is byte-exact regardless.
fn decode_name(bytes: &[u8], gp_flags: u16) -> String {
    // Fast path: assume valid UTF-8 (the common case for every file
    // written by a modern tool). Only fall through to the lossy
    // decoder / CP437 table when the fast path rejects. Previously
    // the UTF-8-flagged branch always went through
    // `String::from_utf8_lossy(..).into_owned()`, which runs a
    // state machine even for valid inputs — then allocates.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    if gp_flags & (1 << 11) != 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    cp437_to_string(bytes)
}

/// Best-effort CP437 → UTF-8. Printable ASCII passes through; high bytes
/// are mapped by a small table matching the IBM PC code page. Not a full
/// implementation — good enough for display in logs. Byte-for-byte
/// round-tripping goes through `name_bytes`, not this.
fn cp437_to_string(bytes: &[u8]) -> String {
    const HI: [char; 128] = [
        'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å',
        'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ',
        'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»',
        '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
        '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧',
        '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀',
        'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
        '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', ' ',
    ];
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x80 {
            s.push(b as char);
        } else {
            s.push(HI[(b - 0x80) as usize]);
        }
    }
    s
}
