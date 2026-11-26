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
use crate::zipformat::{
    AES_EXTRA_ID, METHOD_AES, SIG_CDR, SIG_EOCD, SIG_EOCD64, SIG_EOCD64_LOC, SIG_LFH,
    TZIP_HASH_ALGO_MD5, TZIP_HASH_ALGO_SHA256, TZIP_HASH_EXTRA_ID,
};

const EOCD_SCAN_WINDOW: u64 = 64 * 1024;
const EOCD_MIN_SIZE: u64 = 22;

/// A central-directory entry, parsed from an existing archive. Enough to
/// re-emit as a central entry in a rewritten archive, and enough
/// for the writer's byte-copy path to locate the original LFH +
/// body on disk. Definition moved into `zipformat` so `zipwriter`
/// can consume it without re-importing `zipreader`; re-exported
/// here to keep the historical path (`zipreader::ReadEntry`)
/// usable by existing callers.
pub use crate::zipformat::ReadEntry;

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
    /// Parsed name-index segments (`.tzip/names.v1`, `.tzip/names.v1.seg1`,
    /// `.tzip/names.v1.seg2`, ...). Initial create emits the
    /// unsuffixed form; `tzip u` adds `.segN` segments covering the
    /// entries each append brought in. `lookup_entry_idx` walks all
    /// segments in order. Empty when no index present.
    name_segments: Vec<crate::nameidx::NameIndex>,
    /// Term-index segments. `tzip grep` queries union the posting
    /// lists from every segment before narrowing candidates.
    /// Loaded lazily on first `term_candidates` / `term_coverage`
    /// call — see `term_segments_loaded`; ChatGPT review #5.
    term_segments: Vec<crate::termidx::TermIndex>,
    /// `true` once `try_load_term_segments` has run. Guards the
    /// lazy populate so we don't re-scan the CDR + source every
    /// call. Stays `false` on archives that never run grep.
    term_segments_loaded: bool,
    /// Per-field facet segments. Each field maps to one or more
    /// `Facet` instances; `facet()` merges counts across them.
    /// Loaded lazily — ChatGPT review #5. Only `tzip aggregate`
    /// touches facets, so eager loading was dead I/O for every
    /// other op (which was most of them).
    facet_segments: std::collections::HashMap<String, Vec<crate::facets::Facet>>,
    facet_segments_loaded: bool,
    /// Phase 6 index cache binding. Set via `attach_cache`.
    /// `None` on local archives (no payoff — body-reads are already
    /// a local file-seek) and on remote archives opened without an
    /// `--cache-dir`.
    cache_ctx: Option<CacheCtx>,
    /// Password for AE-2 encrypted `.tzip/*` sentinels. Set via
    /// `attach_password` by the reader runners that have a
    /// password in hand. Without this, `read_index_body`
    /// returns `None` for encrypted sentinels and
    /// `tzip aggregate`/`query`/`jq`/`grep`'s term index all
    /// silently degrade (facet_fields empty, term_candidates
    /// None → full linear scan). PR #69's fix #2 made
    /// sentinels encrypted for security; this field lets
    /// readers un-encrypt them when they have the password.
    archive_password: Option<String>,
    /// ETag / Last-Modified the remote source captured at open
    /// time. Set via `attach_remote_fingerprint` by the reader
    /// runners that pull the probe response off the source
    /// before handing it over. Downstream worker-source
    /// constructors use this as `If-Match` so a mid-run object
    /// replacement surfaces as a 412 instead of silently mixing
    /// bytes from two versions of the archive.
    remote_fingerprint: Option<String>,
}

/// Per-archive cache binding: shared cache handle + this archive's
/// identity within it. Cloneable because the cache handle is an Arc.
#[derive(Clone)]
struct CacheCtx {
    cache: std::sync::Arc<crate::indexcache::IndexCache>,
    archive_id: crate::indexcache::ArchiveId,
}

/// Was a CDR entry covered by the term index when it was built?
/// Used by `tzip grep` to decide whether the candidate set from
/// `term_candidates` is authoritative for that entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermCoverage {
    /// At least one segment recorded this entry in its coverage
    /// trailer. Grep can trust the candidate set — if `idx` isn't
    /// in it, the entry can't match a literal query.
    Covered,
    /// Every segment with a trailer explicitly excluded this
    /// entry (e.g. `.log` in a corpus built with `--index-terms
    /// '*.json'`). Grep must scan it linearly.
    NotCovered,
    /// Legacy segment without a trailer (pre-fix format) OR no
    /// term index at all. Grep must scan conservatively.
    Unknown,
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
            return Self::finish_parse(Box::new(file), file_size, eocd_pos, eocd, None)
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
        let arch = Self::finish_parse(Box::new(file), file_size, eocd_pos, eocd, None)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok((arch, rw_handle))
    }

    /// Open an archive against any `Read + Seek + Send` source. HTTP, S3,
    /// in-memory cursors — anything implementing `ReadSeek`. The source
    /// must be single-disk; multi-disk archives come through `open`
    /// above (local-only, since sibling disks live on the filesystem).
    pub fn open_source(source: Box<dyn ReadSeek>) -> Result<Self> {
        Self::open_source_with_cache(source, None)
    }

    /// Same as `open_source` but with an optional Phase-6 index
    /// cache + archive identity attached before `.tzip/*.v1` index
    /// entries get loaded. Passing `Some((cache, id))` turns repeat
    /// opens of the same remote archive into cache-hit reads —
    /// no Range GET for already-seen index sentinels.
    pub fn open_source_with_cache(
        mut source: Box<dyn ReadSeek>,
        cache_binding: Option<(
            std::sync::Arc<crate::indexcache::IndexCache>,
            crate::indexcache::ArchiveId,
        )>,
    ) -> Result<Self> {
        let file_size = source
            .seek(SeekFrom::End(0))
            .context("seek to end of archive source")?;
        if file_size < EOCD_MIN_SIZE {
            bail!("archive is too small to be a ZIP ({} bytes)", file_size);
        }

        // ChatGPT-review #74 / issue #74: before issuing the
        // EOCD scan + CDR range GET (which dominates the wire
        // cost on million-entry archives — ~70 bytes × entries),
        // check the Phase 6 cache for a cached CDR blob keyed
        // on (url, fingerprint). On hit we skip every remote
        // read past this point.
        if let Some((cache, id)) = &cache_binding {
            if let Some(blob) = crate::cdr_cache::load(cache, id) {
                // Defence-in-depth: an attacker who can write to
                // the local cache dir shouldn't be able to point
                // us at arbitrary byte ranges past end-of-file.
                // Reject mismatches rather than trust the cache
                // blindly.
                if blob.file_size == file_size
                    && blob
                        .cd_offset
                        .checked_add(blob.cd_size)
                        .is_some_and(|end| end <= blob.file_size)
                    && blob.cdr.len() as u64 == blob.cd_size
                {
                    let entries =
                        parse_central_directory(&blob.cdr, blob.entry_count)
                            .context("parse CDR from cache blob")?;
                    let tail_size = blob.file_size - blob.cd_offset;
                    let archive_comment = decode_name(&blob.comment, 0);
                    let mut arch = Self {
                        source,
                        entries,
                        file_size: blob.file_size,
                        cd_offset: blob.cd_offset,
                        cd_size: blob.cd_size,
                        tail_size,
                        archive_comment,
                        disk_count: 1,
                        name_segments: Vec::new(),
                        term_segments: Vec::new(),
                        term_segments_loaded: false,
                        facet_segments: std::collections::HashMap::new(),
                        facet_segments_loaded: false,
                        cache_ctx: Some(CacheCtx {
                            cache: std::sync::Arc::clone(cache),
                            archive_id: id.clone(),
                        }),
                        archive_password: None,
                        remote_fingerprint: None,
                    };
                    arch.try_load_name_segments();
                    return Ok(arch);
                }
                // Blob rejected (mismatched size / bad fields).
                // Fall through to a real fetch; the stale blob
                // gets overwritten by the writeback below.
            }
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

        Self::finish_parse(source, file_size, eocd_pos, eocd, cache_binding)
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
        cache_binding: Option<(
            std::sync::Arc<crate::indexcache::IndexCache>,
            crate::indexcache::ArchiveId,
        )>,
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

        // Write the CDR blob back into the Phase 6 cache (issue
        // #74), keyed on the same (url, fingerprint) as sentinel
        // entries. Soft-fails — a failed cache write never
        // aborts the open; the next open just re-fetches.
        if let Some((cache, id)) = &cache_binding {
            let blob = crate::cdr_cache::Blob {
                file_size,
                cd_offset,
                cd_size,
                entry_count,
                comment: eocd.comment_bytes.clone(),
                cdr: cdr_bytes.clone(),
            };
            crate::cdr_cache::store(cache, id, &blob);
        }

        let tail_size = file_size - cd_offset;
        let archive_comment = decode_name(&eocd.comment_bytes, 0);
        let mut arch = Self {
            source,
            entries,
            file_size,
            cd_offset,
            cd_size,
            tail_size,
            archive_comment,
            disk_count: 1,
            name_segments: Vec::new(),
            term_segments: Vec::new(),
            term_segments_loaded: false,
            facet_segments: std::collections::HashMap::new(),
            facet_segments_loaded: false,
            cache_ctx: cache_binding.map(|(cache, archive_id)| CacheCtx { cache, archive_id }),
            archive_password: None,
            remote_fingerprint: None,
        };
        arch.try_load_name_segments();
        // term + facet segments load lazily on first access —
        // ChatGPT review #5. Most ops (`ls`, `x`, `cat`, `query`)
        // never touch them; eager loading was pure dead I/O on
        // remote archives.
        Ok(arch)
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
        let mut arch = Self {
            source,
            entries,
            file_size,
            cd_offset,
            cd_size,
            tail_size,
            archive_comment,
            disk_count,
            name_segments: Vec::new(),
            term_segments: Vec::new(),
            term_segments_loaded: false,
            facet_segments: std::collections::HashMap::new(),
            facet_segments_loaded: false,
            cache_ctx: None,
            archive_password: None,
            remote_fingerprint: None,
        };
        arch.try_load_name_segments();
        // See primary `open_source_with_cache` for why term + facet
        // are not eagerly loaded here either.
        Ok(arch)
    }

    /// Number of physical disks this archive is spanned across. 1 for a
    /// standard single-file ZIP; `n ≥ 2` only for multi-disk archives
    /// opened through `Archive::open`. Lets extract/grep skip a
    /// redundant EOCD probe when fanning out worker sources.
    /// Look up an entry index by name. Walks every name-index
    /// segment in append-order; returns the first hit. Falls back
    /// to a linear CDR scan when no segment matches (strictly
    /// correct for archives without any name index). `tzip u`
    /// appends segments as new entries land, so lookups stay
    /// coherent across incremental updates.
    pub fn lookup_entry_idx(&self, name: &str) -> Option<usize> {
        for seg in &self.name_segments {
            if let Some(idx) = seg.lookup(name) {
                return Some(idx as usize);
            }
        }
        self.entries.iter().position(|e| e.name == name)
    }

    /// Enumerate name-index segments: the unsuffixed
    /// `.tzip/names.v1` from the initial create, plus any
    /// `.tzip/names.v1.segN` written by subsequent `tzip u` calls,
    /// sorted by segment number.
    fn try_load_name_segments(&mut self) {
        let bodies = self.read_segment_bodies(crate::nameidx::INDEX_ENTRY_NAME);
        for body in bodies {
            if let Ok(ni) = crate::nameidx::NameIndex::parse(&body) {
                self.name_segments.push(ni);
            }
        }
    }

    /// Same for term-index segments. Each segment is independently
    /// built; a query unions posting lists across segments to get
    /// the final candidate set.
    fn try_load_term_segments(&mut self) {
        let bodies = self.read_segment_bodies(crate::termidx::INDEX_ENTRY_NAME);
        for body in bodies {
            if let Ok(ti) = crate::termidx::TermIndex::parse(&body) {
                self.term_segments.push(ti);
            }
        }
    }

    /// Enumerate every `.tzip/columnar.v1.parquet[.segN]` body in
    /// segment-number order (Phase 5b). Query dispatch stages all
    /// of these to temp files and hands the list to DuckDB's
    /// `read_parquet([...])` for a transparent UNION.
    ///
    /// Returns an empty Vec when the archive has no columnar
    /// sidecar at all; a single-element Vec for the Phase-5 base
    /// case; a multi-element Vec for archives with Phase-5b
    /// segments. Readers don't need to care which case applies.
    pub fn columnar_segment_bodies(&mut self) -> Vec<Vec<u8>> {
        self.read_segment_bodies(crate::columnar::INDEX_ENTRY_NAME)
    }

    /// `(name, body_offset, body_len)` triples for every columnar
    /// sidecar segment the archive carries, in segment-number
    /// order. Used by `tzip query --sql`'s range-read prune path
    /// (ChatGPT review #4) — it lets the pruner open a fresh
    /// `ReadSeek` source and range-read just the footer + the
    /// row groups that survived predicate pruning, instead of
    /// pulling every sidecar byte off the wire through
    /// `columnar_segment_bodies`.
    ///
    /// Returns `None` for any segment whose entry is compressed
    /// or encrypted — the range-read trick only works on Store
    /// entries (the body bytes are the raw Parquet file), so the
    /// caller falls back to the full-body path for those.
    pub fn columnar_segment_ranges(&mut self) -> Vec<Option<(String, u64, u64)>> {
        let seg_prefix = format!("{}.seg", crate::columnar::INDEX_ENTRY_NAME);
        let base = crate::columnar::INDEX_ENTRY_NAME;
        let mut hits: Vec<(u32, usize)> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            if e.name == base {
                hits.push((0, i));
            } else if let Some(rest) = e.name.strip_prefix(&seg_prefix) {
                if let Ok(n) = rest.parse::<u32>() {
                    if n > 0 {
                        hits.push((n, i));
                    }
                }
            }
        }
        hits.sort_by_key(|(n, _)| *n);
        let mut out: Vec<Option<(String, u64, u64)>> = Vec::with_capacity(hits.len());
        for (_, idx) in hits {
            // Need the LFH size before we can compute body offset.
            if self.lfh_total_size_of(idx).is_err() {
                out.push(None);
                continue;
            }
            let re = &self.entries[idx];
            if re.method_on_wire != 0 || re.encrypted {
                out.push(None);
                continue;
            }
            let body_off = re.lfh_offset + re.lfh_total_size as u64;
            out.push(Some((re.name.clone(), body_off, re.comp_size)));
        }
        out
    }

    /// For each unsuffixed base name `.tzip/*.v1`, return the
    /// bodies of the base entry and every `.segN` sibling in
    /// segment-number order. Readers parse each into whatever
    /// typed index they manage.
    fn read_segment_bodies(&mut self, base: &str) -> Vec<Vec<u8>> {
        let seg_prefix = format!("{}.seg", base);
        let mut hits: Vec<(u32, String)> = Vec::new();
        for e in &self.entries {
            if e.name == base {
                hits.push((0, e.name.clone()));
            } else if let Some(rest) = e.name.strip_prefix(&seg_prefix) {
                if let Ok(n) = rest.parse::<u32>() {
                    // seg0 is reserved for the unsuffixed form;
                    // explicit .seg0 collides. Skip to avoid duplicates.
                    if n > 0 {
                        hits.push((n, e.name.clone()));
                    }
                }
            }
        }
        hits.sort_by_key(|(n, _)| *n);
        let mut bodies = Vec::with_capacity(hits.len());
        for (_, name) in hits {
            if let Some(body) = self.read_index_body(&name) {
                bodies.push(body);
            }
        }
        bodies
    }

    /// Shared reader for `.tzip/*.v1` sentinel entries. They're always
    /// written as uncompressed, unencrypted Store entries so we can
    /// read them directly off the source without going through the
    /// decompress stack. Returns `None` on any failure so callers
    /// just skip the feature.
    ///
    /// When an index cache is attached (Phase 6), consults it before
    /// touching the underlying source and writes back on a miss.
    /// Cache hits turn `.tzip/*.v1` reads from "Range GET over the
    /// network" into "fs::read of a local file" — the mechanism that
    /// gets a repeated remote aggregate / query under the roadmap's
    /// p99 target.
    fn read_index_body(&mut self, name: &str) -> Option<Vec<u8>> {
        if let Some(ctx) = &self.cache_ctx {
            if let Some(body) = ctx.cache.get(&ctx.archive_id, name) {
                return Some(body);
            }
        }
        let idx = self.entries.iter().position(|e| e.name == name)?;
        self.lfh_total_size_of(idx).ok()?;
        let re = &self.entries[idx];
        let body_off = re.lfh_offset + re.lfh_total_size as u64;
        let method = re.method_on_wire;
        let encrypted = re.encrypted;
        let comp_size = re.comp_size;
        // Case 1: plain Store sentinel (original Phase-6 shape).
        if method == 0 && !encrypted {
            let mut body = vec![0u8; comp_size as usize];
            self.source
                .seek(SeekFrom::Start(body_off))
                .and_then(|_| self.source.read_exact(&mut body))
                .ok()?;
            if let Some(ctx) = &self.cache_ctx {
                let _ = ctx.cache.put(&ctx.archive_id, name, &body);
            }
            return Some(body);
        }
        // Case 2: AE-2 encrypted sentinel (PR #69's fix #2). The
        // encrypted on-wire form is Store-wrapped-in-AES, i.e.
        // method_on_wire = 99 (METHOD_AES), inner_method = 0.
        // Decrypt back to plaintext using the attached password;
        // caller is `aggregate` / `query` / `jq` / `grep` with a
        // `--password` in hand.
        if encrypted && method == crate::zipformat::METHOD_AES {
            let pw = self.archive_password.clone()?;
            let mut ct = vec![0u8; comp_size as usize];
            self.source
                .seek(SeekFrom::Start(body_off))
                .and_then(|_| self.source.read_exact(&mut ct))
                .ok()?;
            // `comp_size` on an AE-2 entry is the full envelope:
            // salt (16) + verify (2) + ciphertext + mac10. The
            // plaintext length = comp_size - AE2_OVERHEAD.
            let overhead = crate::crypto::AE2_OVERHEAD;
            if ct.len() < overhead {
                return None;
            }
            // decrypt_ae2's `ciphertext_len` parameter is the
            // *ciphertext* byte count, not the full envelope.
            // Envelope layout: salt(16) + verify(2) + ciphertext
            // + mac10 → ciphertext_len = comp_size - overhead.
            let plain_len = ct.len() - overhead;
            let mut plain = Vec::with_capacity(plain_len);
            let mut cursor = std::io::Cursor::new(&ct[..]);
            crate::crypto::decrypt_ae2(&pw, &mut cursor, plain_len as u64, &mut plain).ok()?;
            if let Some(ctx) = &self.cache_ctx {
                // Cache the *decrypted* body — the cache is
                // local and already 0600-protected (PR #91).
                let _ = ctx.cache.put(&ctx.archive_id, name, &plain);
            }
            return Some(plain);
        }
        // Everything else (compressed sentinel without encryption,
        // ZipCrypto-style encryption, etc.) isn't a sentinel we
        // currently write — leave the None return intact.
        None
    }

    /// Attach a Phase-6 index cache to this archive. All subsequent
    /// `.tzip/*.v1[*.segN]` reads consult the cache first and
    /// populate on miss. Caller is responsible for deriving the
    /// `ArchiveId` from the source's URL + content fingerprint
    /// (ETag / Last-Modified / Content-Length) so stale buckets
    /// age out instead of serving wrong bytes.
    /// Attach a password so `read_index_body` can decrypt
    /// AE-2-encrypted `.tzip/*` sentinels on password-
    /// protected archives (PR #69 fix #2 made sentinels
    /// encrypted). Callers that already have a `--password`
    /// in hand (grep, aggregate, query, jq, cat, x) should
    /// call this immediately after opening the archive so
    /// lazy-loaded sentinels (term / facet / columnar) and
    /// the name index all see the password on first access.
    ///
    /// Re-loads name segments after attach, because those
    /// are populated eagerly at open time — if the open
    /// predated the password attach, the name-index entry
    /// will have returned None and left `name_segments`
    /// empty.
    pub fn attach_password(&mut self, password: String) {
        self.archive_password = Some(password);
        if self.name_segments.is_empty() {
            self.try_load_name_segments();
        }
        // term / facet / columnar sentinels load lazily
        // through their `ensure_*_segments` paths — they'll
        // pick up the password on first access.
        self.term_segments_loaded = false;
        self.facet_segments_loaded = false;
    }

    pub fn attach_cache(
        &mut self,
        cache: std::sync::Arc<crate::indexcache::IndexCache>,
        archive_id: crate::indexcache::ArchiveId,
    ) {
        self.cache_ctx = Some(CacheCtx { cache, archive_id });
    }

    /// Store the remote source's ETag / Last-Modified for later
    /// use as a Range-GET precondition (`If-Match`/
    /// `If-Unmodified-Since`). Called by the reader runners that
    /// opened the remote source; the fingerprint is read back via
    /// `remote_fingerprint()` when spawning per-worker sources so
    /// every worker pins to the snapshot the main thread saw.
    pub fn attach_remote_fingerprint(&mut self, fp: String) {
        self.remote_fingerprint = Some(fp);
    }

    /// The remote ETag / Last-Modified captured at open time, or
    /// `None` on local archives and on remote sources that
    /// didn't emit either. Used by workers to pin their own
    /// Range GETs to the same snapshot the main thread opened.
    pub fn remote_fingerprint(&self) -> Option<&str> {
        self.remote_fingerprint.as_deref()
    }

    /// Try to load every `.tzip/facets.v1/<leaf>[.segN]` entry as a
    /// Facet and group them by leaf. Multiple segments per field
    /// are summed on-demand at query time.
    fn try_load_facet_segments(&mut self) {
        // Snapshot the list of facet entry names before taking a
        // mutable borrow on `self.source` to read bodies.
        let targets: Vec<(usize, String)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| crate::facets::is_facet_entry_name(&e.name))
            .map(|(i, e)| (i, e.name.clone()))
            .collect();
        // Group by base-leaf and sort by `.segN` suffix so segments
        // merge in deterministic order.
        let mut by_leaf: std::collections::HashMap<String, Vec<(u32, String)>> =
            std::collections::HashMap::new();
        for (_i, name) in &targets {
            // Strip `.tzip/facets.v1/` prefix to get `<leaf>` or
            // `<leaf>.segN`.
            let after_prefix = match name.strip_prefix(".tzip/facets.v1/") {
                Some(s) => s,
                None => continue,
            };
            let (leaf, seg) = match after_prefix.rsplit_once(".seg") {
                Some((leaf, num)) => match num.parse::<u32>() {
                    Ok(n) => (leaf.to_string(), n),
                    Err(_) => (after_prefix.to_string(), 0),
                },
                None => (after_prefix.to_string(), 0),
            };
            by_leaf
                .entry(leaf)
                .or_insert_with(Vec::new)
                .push((seg, name.clone()));
        }
        for (leaf, mut segs) in by_leaf {
            segs.sort_by_key(|(n, _)| *n);
            let mut facets: Vec<crate::facets::Facet> = Vec::with_capacity(segs.len());
            for (_, name) in &segs {
                if let Some(body) = self.read_index_body(name) {
                    if let Ok(f) = crate::facets::Facet::parse(&body) {
                        facets.push(f);
                    }
                }
            }
            if !facets.is_empty() {
                self.facet_segments.insert(leaf, facets);
            }
        }
    }

    /// Lazy-load the facet segments if they haven't been yet. Idempotent.
    /// Callers that need facets go through this (directly or via `facet*`);
    /// callers that don't pay nothing at open time. ChatGPT review #5.
    fn ensure_facet_segments(&mut self) {
        if !self.facet_segments_loaded {
            self.try_load_facet_segments();
            self.facet_segments_loaded = true;
        }
    }

    /// Lazy-load the term segments if they haven't been yet. Idempotent.
    fn ensure_term_segments(&mut self) {
        if !self.term_segments_loaded {
            self.try_load_term_segments();
            self.term_segments_loaded = true;
        }
    }

    /// Available facet field names (leaves), alphabetic order.
    pub fn facet_fields(&mut self) -> Vec<String> {
        self.ensure_facet_segments();
        let mut names: Vec<String> = self.facet_segments.keys().cloned().collect();
        names.sort();
        names
    }

    /// Return a merged `Facet` for `field`, summing counts across
    /// every segment this archive carries. `None` if no segment has
    /// the field. Appendable-safe: `tzip u` can write additional
    /// `.segN` entries and they fold into the returned total.
    pub fn facet(&mut self, field: &str) -> Option<crate::facets::Facet> {
        self.ensure_facet_segments();
        let segs = self.facet_segments.get(field)?;
        if segs.is_empty() {
            return None;
        }
        let mut merged = crate::facets::Facet {
            field: field.to_string(),
            total: 0,
            counts: std::collections::HashMap::new(),
        };
        for s in segs {
            merged.total += s.total;
            for (k, v) in &s.counts {
                *merged.counts.entry(k.clone()).or_insert(0) += *v;
            }
        }
        Some(merged)
    }

    /// Entry indices that could contain every literal token in
    /// `query`. Unions candidate sets from every term-index segment
    /// (each segment covers a disjoint range of CDR positions, by
    /// construction of the append flow). Returns:
    /// - `Some(Some(set))` when at least one segment applied.
    /// - `Some(None)` when every segment said the query has no
    ///   usable tokens — caller should fall through to full scan.
    /// - `None` when the archive has no term index at all.
    pub fn term_candidates(
        &mut self,
        query: &str,
    ) -> Option<Option<std::collections::HashSet<u32>>> {
        self.ensure_term_segments();
        if self.term_segments.is_empty() {
            return None;
        }
        let mut any_usable = false;
        let mut merged: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for seg in &self.term_segments {
            match seg.candidates(query) {
                Some(set) => {
                    any_usable = true;
                    for v in set {
                        merged.insert(v);
                    }
                }
                None => {
                    // This segment can't help (no usable tokens).
                    // Other segments might still help for the same
                    // query (no reason they would with the same
                    // tokenizer, but leave the check honest).
                }
            }
        }
        Some(if any_usable { Some(merged) } else { None })
    }

    /// Does the term index actually cover CDR entry `idx`? Phase-2
    /// indices only ingested bodies matching `--index-terms` at
    /// create time, so a `.log` entry in a corpus built with
    /// `--index-terms '*.json'` is never mentioned in any posting
    /// list — but it still exists in the archive and must be
    /// scanned for literal matches. `term_candidates` alone would
    /// silently exclude it.
    ///
    /// Returns:
    ///
    /// - `Coverage::Covered` when at least one term segment
    ///   recorded `idx` in its coverage trailer.
    /// - `Coverage::NotCovered` when every segment that carried a
    ///   trailer explicitly excluded `idx` (and no segment was
    ///   legacy).
    /// - `Coverage::Unknown` when any segment predates the
    ///   trailer, or when the archive has no term index at all —
    ///   the caller must scan `idx` conservatively.
    pub fn term_coverage(&mut self, idx: u32) -> TermCoverage {
        self.ensure_term_segments();
        if self.term_segments.is_empty() {
            return TermCoverage::Unknown;
        }
        let mut any_covered = false;
        let mut any_legacy = false;
        for seg in &self.term_segments {
            match seg.covers(idx) {
                Some(true) => any_covered = true,
                Some(false) => {}
                None => any_legacy = true,
            }
        }
        if any_covered {
            TermCoverage::Covered
        } else if any_legacy {
            TermCoverage::Unknown
        } else {
            TermCoverage::NotCovered
        }
    }

    pub fn disk_count(&self) -> usize {
        self.disk_count
    }

    /// Archive-level comment from the EOCD trailer. Empty string if the
    /// archive has no comment.
    pub fn archive_comment(&self) -> &str {
        &self.archive_comment
    }

    /// Stream the `idx`-th entry's body — LFH skip + decrypt +
    /// decompress — into `dst`. Narrow "please read this entry"
    /// operation that the Archive handles end-to-end; callers
    /// never touch the underlying source handle.
    pub fn stream_entry_to<W: std::io::Write>(
        &mut self,
        idx: usize,
        password: Option<&str>,
        dst: &mut W,
    ) -> Result<u64> {
        self.lfh_total_size_of(idx)?;
        let re = self.entries[idx].clone();
        stream_entry_source_to(&mut *self.source, &re, password, dst)
    }

    /// Copy one entry verbatim (LFH + extras + body) into `zw` at
    /// the writer's current position. Used by rebuild-mode update
    /// to carry kept entries without decoding them. The Archive
    /// holds the source; the writer holds the destination; this
    /// method's the only thing that needs to touch both at once,
    /// so it owns the end-to-end operation and keeps
    /// `Archive::source` fully private.
    pub fn carry_entry_to<W: std::io::Write + std::io::Seek>(
        &mut self,
        re: &crate::zipformat::ReadEntry,
        zw: &mut crate::zipwriter::ZipWriter<W>,
    ) -> Result<()> {
        zw.carry_existing_entry(&mut *self.source, re)
    }

    /// Single-entry view by index, panicking on out-of-range.
    /// Callers that take a fallible lookup go through
    /// `lookup_entry_idx` first.
    pub fn entry(&self, idx: usize) -> &ReadEntry {
        &self.entries[idx]
    }

    /// Entry count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the archive holds zero entries (empty ZIP).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate every entry in CDR order.
    pub fn iter_entries(&self) -> impl Iterator<Item = &ReadEntry> {
        self.entries.iter()
    }

    /// Names of every entry in CDR order — copies into a Vec so
    /// callers can hold it without borrowing the archive. Used by
    /// extract / hash / update to resolve `--include`/`--exclude`
    /// matches without pulling the full `ReadEntry` out.
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
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

/// Stream one entry's body (decrypted if needed, decompressed) from
/// an already-open `ReadSeek` source to `dst`. Used by both
/// `Archive::stream_entry_to` and by parallel grep / extract workers
/// that hold their own per-worker source handles.
/// `re.lfh_total_size` must already be populated (via
/// `Archive::lfh_total_size_of`).
pub(crate) fn stream_entry_source_to<W: std::io::Write>(
    source: &mut dyn ReadSeek,
    re: &ReadEntry,
    password: Option<&str>,
    dst: &mut W,
) -> Result<u64> {
    let body_offset = re.lfh_offset + re.lfh_total_size as u64;
    source
        .seek(SeekFrom::Start(body_offset))
        .with_context(|| format!("seek to body of {}", re.name))?;

    let inner_method = method_from_id(re.inner_method, re.gp_flags)
        .ok_or_else(|| anyhow!("unsupported compression method {} on {}", re.inner_method, re.name))?;

    if re.encrypted {
        let password = password.ok_or_else(|| {
            anyhow!(
                "entry {} is encrypted — pass --password / -p (or - to prompt)",
                re.name
            )
        })?;
        if re.method_on_wire == METHOD_AES {
            // WinZip AES-256 AE-2.
            let ciphertext_len = re
                .comp_size
                .checked_sub(crate::crypto::AE2_OVERHEAD as u64)
                .ok_or_else(|| anyhow!("entry {} too small to be AE-2", re.name))?;
            let mut plaintext: Vec<u8> =
                Vec::with_capacity(re.uncomp_size.min(64 * 1024 * 1024) as usize);
            crate::crypto::decrypt_ae2(password, source, ciphertext_len, &mut plaintext)?;
            crate::decompress::decompress(
                inner_method,
                std::io::Cursor::new(&plaintext),
                re.uncomp_size,
                dst,
            )
        } else {
            // Traditional PKZIP encryption (ZipCrypto). The 12-byte
            // encryption header is baked into the comp_size, so
            // after ZipCryptoReader consumes it the decompressor
            // sees comp_size - 12 bytes of plaintext.
            let verify_byte = zipcrypto_verify_byte(re);
            let bounded = source.take(re.comp_size);
            let decrypted =
                crate::zipcrypto::ZipCryptoReader::new(bounded, password.as_bytes(), verify_byte)
                    .with_context(|| format!("decrypt ZipCrypto entry {}", re.name))?;
            crate::decompress::decompress(inner_method, decrypted, re.uncomp_size, dst)
        }
    } else {
        let bounded = source.take(re.comp_size);
        crate::decompress::decompress(inner_method, bounded, re.uncomp_size, dst)
    }
}

/// The verify byte ZipCrypto compares against the last plaintext
/// byte of the 12-byte encryption header.
fn zipcrypto_verify_byte(re: &ReadEntry) -> u8 {
    const GP_BIT_STREAMING_DATA_DESCRIPTOR: u16 = 1 << 3;
    if re.gp_flags & GP_BIT_STREAMING_DATA_DESCRIPTOR != 0 {
        ((re.mtime.1 >> 8) & 0xff) as u8
    } else {
        ((re.crc32 >> 24) & 0xff) as u8
    }
}

/// Resolve a ZIP compression method ID + GP flag word to the
/// read-side enum.
pub(crate) fn method_from_id(id: u16, gp_flags: u16) -> Option<crate::decompress::ReadMethod> {
    use crate::decompress::ReadMethod;
    Some(match id {
        0 => ReadMethod::Store,
        1 => ReadMethod::Shrink,
        2 => ReadMethod::Reduce(1),
        3 => ReadMethod::Reduce(2),
        4 => ReadMethod::Reduce(3),
        5 => ReadMethod::Reduce(4),
        6 => ReadMethod::Implode {
            large_window: (gp_flags & 0b0000_0010) != 0,
            lit_tree: (gp_flags & 0b0000_0100) != 0,
        },
        8 => ReadMethod::Deflate,
        9 => ReadMethod::Deflate64,
        12 => ReadMethod::Bzip2,
        14 => ReadMethod::Lzma,
        93 => ReadMethod::Zstd,
        95 => ReadMethod::Xz,
        98 => ReadMethod::Ppmd,
        _ => return None,
    })
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
/// (bytes starting at `cd_offset` — the CDR, then the optional
/// ZIP64 EOCD + ZIP64 locator, then the EOCD). Used by append-
/// mode update's undo snapshot: the caller already has the tail
/// in hand, and shouldn't have to re-materialise a scratch file
/// on disk to reconstruct the entry list.
///
/// Handles both classic and ZIP64 tails. `cd_offset` is the
/// archive-absolute offset the tail starts at — needed to map
/// the ZIP64 locator's absolute `z64_off` back into the tail.
///
/// Multi-disk tails still fall through to the rebuild branch of
/// update (those are vanishingly rare in practice and the
/// append path would also have to teach `ZipWriter` about
/// multi-disk start offsets).
pub fn parse_entries_from_tail(tail: &[u8], cd_offset: u64) -> Result<Vec<ReadEntry>> {
    use std::io::Cursor;
    let tail_len = tail.len() as u64;
    if tail_len < EOCD_MIN_SIZE {
        bail!("archive tail too small to be a ZIP ({} bytes)", tail_len);
    }
    let mut cur = Cursor::new(tail);
    let (eocd_pos, eocd) = find_eocd(&mut cur, tail_len).context("find EOCD in tail")?;
    if eocd.this_disk != 0 || eocd.cd_disk != 0 {
        bail!("multi-disk tail parse not supported via parse_entries_from_tail");
    }
    let (entry_count, cd_size): (u64, usize) = if eocd.needs_zip64() {
        // The EOCD64 locator is the 20 bytes immediately before
        // the EOCD. Use its absolute `z64_off` to find the
        // EOCD64 record within the tail — deterministic, no
        // backward signature scan.
        if eocd_pos < 20 {
            bail!("no room for EOCD64 locator before EOCD in tail");
        }
        let loc_start = (eocd_pos - 20) as usize;
        let loc: &[u8] = &tail[loc_start..loc_start + 20];
        let sig = u32::from_le_bytes([loc[0], loc[1], loc[2], loc[3]]);
        if sig != SIG_EOCD64_LOC {
            bail!("bad EOCD64 locator signature in tail: {:#x}", sig);
        }
        let z64_abs = u64::from_le_bytes(loc[8..16].try_into().unwrap());
        if z64_abs < cd_offset {
            bail!(
                "EOCD64 at {:#x} precedes cd_offset {:#x}",
                z64_abs,
                cd_offset
            );
        }
        let z64_tail_pos = (z64_abs - cd_offset) as usize;
        if z64_tail_pos + 56 > tail.len() {
            bail!("EOCD64 header extends past end of tail");
        }
        let hdr: &[u8] = &tail[z64_tail_pos..z64_tail_pos + 56];
        let z64_sig = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if z64_sig != SIG_EOCD64 {
            bail!("bad EOCD64 signature in tail: {:#x}", z64_sig);
        }
        let entry_count = u64::from_le_bytes(hdr[32..40].try_into().unwrap());
        let cd_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        (entry_count, cd_size as usize)
    } else {
        (eocd.entry_count as u64, eocd.cd_size as usize)
    };
    // Tail starts at cd_offset in the real archive, so the CDR
    // lives at [0 .. cd_size] within `tail`.
    if cd_size > tail.len() {
        bail!(
            "tail shorter than declared CDR ({} < {})",
            tail.len(),
            cd_size
        );
    }
    parse_central_directory(&tail[..cd_size], entry_count)
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
        let mut md5_digest: Option<[u8; 16]> = None;
        let mut sha256_digest: Option<[u8; 32]> = None;

        // Walk extras: ZIP64 (0x0001), AES (0x9901), tzip-hash (0x7A68).
        // Unknown extras are skipped (we only need to round-trip to re-emit).
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
            } else if id == TZIP_HASH_EXTRA_ID && !body.is_empty() {
                // tzip-native plaintext digest. Layout: [algo: u8] [digest].
                // algo=1 → MD5 (16 bytes), algo=2 → SHA-256 (32 bytes).
                // Multiple per entry are allowed (one for md5, one for sha256).
                match body[0] {
                    TZIP_HASH_ALGO_MD5 if body.len() >= 1 + 16 => {
                        let mut buf = [0u8; 16];
                        buf.copy_from_slice(&body[1..1 + 16]);
                        md5_digest = Some(buf);
                    }
                    TZIP_HASH_ALGO_SHA256 if body.len() >= 1 + 32 => {
                        let mut buf = [0u8; 32];
                        buf.copy_from_slice(&body[1..1 + 32]);
                        sha256_digest = Some(buf);
                    }
                    _ => {}
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
            md5_digest,
            sha256_digest,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ZIP64 archive tail (no CDR entries) and
    /// verify `parse_entries_from_tail` recognises it instead of
    /// bailing with "ZIP64 tail parse not supported" (issue #80).
    ///
    /// Layout (98 bytes total):
    ///   [0..56]   EOCD64 (empty CDR)
    ///   [56..76]  EOCD64 locator — absolute z64_off points at
    ///             (cd_offset + 0) i.e. the start of the tail
    ///   [76..98]  EOCD with ZIP64 sentinels
    #[test]
    fn parse_entries_from_tail_handles_zip64_empty_cdr() {
        let cd_offset: u64 = 5_000_000_000; // > 4 GiB, forces ZIP64 in a real archive
        let z64_abs: u64 = cd_offset; // EOCD64 is right after a 0-byte CDR

        let mut tail: Vec<u8> = Vec::with_capacity(98);

        // EOCD64 record (56 bytes).
        tail.extend_from_slice(&SIG_EOCD64.to_le_bytes()); //  0..4
        tail.extend_from_slice(&44u64.to_le_bytes()); //       4..12  size of rec minus leading 12
        tail.extend_from_slice(&45u16.to_le_bytes()); //      12..14  version made by
        tail.extend_from_slice(&45u16.to_le_bytes()); //      14..16  version needed
        tail.extend_from_slice(&0u32.to_le_bytes()); //       16..20  disk number
        tail.extend_from_slice(&0u32.to_le_bytes()); //       20..24  disk w/ CD
        tail.extend_from_slice(&0u64.to_le_bytes()); //       24..32  entries on this disk
        tail.extend_from_slice(&0u64.to_le_bytes()); //       32..40  total entries
        tail.extend_from_slice(&0u64.to_le_bytes()); //       40..48  cd_size
        tail.extend_from_slice(&cd_offset.to_le_bytes()); //  48..56  cd_offset

        // EOCD64 locator (20 bytes).
        tail.extend_from_slice(&SIG_EOCD64_LOC.to_le_bytes()); // 0..4
        tail.extend_from_slice(&0u32.to_le_bytes()); //           4..8  disk w/ z64
        tail.extend_from_slice(&z64_abs.to_le_bytes()); //        8..16 absolute z64_off
        tail.extend_from_slice(&1u32.to_le_bytes()); //          16..20 total disks

        // EOCD (22 bytes, ZIP64 sentinels so needs_zip64() fires).
        tail.extend_from_slice(&SIG_EOCD.to_le_bytes()); //        0..4
        tail.extend_from_slice(&0u16.to_le_bytes()); //            4..6  this disk
        tail.extend_from_slice(&0u16.to_le_bytes()); //            6..8  disk w/ CD
        tail.extend_from_slice(&0xFFFF_u16.to_le_bytes()); //      8..10 ← ZIP64 sentinel
        tail.extend_from_slice(&0xFFFF_u16.to_le_bytes()); //     10..12 ← ZIP64 sentinel
        tail.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes()); // 12..16 ← ZIP64 sentinel
        tail.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes()); // 16..20 ← ZIP64 sentinel
        tail.extend_from_slice(&0u16.to_le_bytes()); //           20..22 comment length

        assert_eq!(tail.len(), 98);
        let got = parse_entries_from_tail(&tail, cd_offset).expect("zip64 tail parsed");
        assert!(
            got.is_empty(),
            "expected 0 entries (empty CDR), got {}",
            got.len()
        );
    }

    /// The post-#80 parser still needs cd_offset to be plausible;
    /// a locator claiming z64 at an offset below cd_offset must
    /// error instead of underflowing.
    #[test]
    fn parse_entries_from_tail_rejects_zip64_locator_below_cd_offset() {
        let cd_offset: u64 = 5_000_000_000;
        let bogus_z64_abs: u64 = cd_offset - 1_000;

        let mut tail = Vec::with_capacity(98);
        // 56-byte EOCD64 with signature + plausible fields.
        tail.extend_from_slice(&SIG_EOCD64.to_le_bytes());
        tail.extend_from_slice(&44u64.to_le_bytes());
        tail.extend_from_slice(&45u16.to_le_bytes());
        tail.extend_from_slice(&45u16.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        tail.extend_from_slice(&0u64.to_le_bytes());
        tail.extend_from_slice(&0u64.to_le_bytes());
        tail.extend_from_slice(&0u64.to_le_bytes());
        tail.extend_from_slice(&cd_offset.to_le_bytes());
        // Locator with a z64_off *before* cd_offset — nonsensical.
        tail.extend_from_slice(&SIG_EOCD64_LOC.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        tail.extend_from_slice(&bogus_z64_abs.to_le_bytes());
        tail.extend_from_slice(&1u32.to_le_bytes());
        // EOCD with ZIP64 sentinels.
        tail.extend_from_slice(&SIG_EOCD.to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());
        tail.extend_from_slice(&0xFFFF_u16.to_le_bytes());
        tail.extend_from_slice(&0xFFFF_u16.to_le_bytes());
        tail.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes());
        tail.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());

        let err = parse_entries_from_tail(&tail, cd_offset)
            .expect_err("locator below cd_offset should error");
        let msg = format!("{}", err);
        assert!(
            msg.contains("precedes cd_offset") || msg.contains("locator"),
            "unexpected error: {}",
            msg
        );
    }

}
