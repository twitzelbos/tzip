//! `tzip u` append mode for `s3://` archives — issue #68.
//!
//! The local append path truncates at the old `cd_offset`, writes
//! new LFH+body records and a rebuilt central directory onto the
//! same file descriptor. S3 objects are immutable, so we do the
//! morally-equivalent dance via multipart upload with server-side
//! part-copy: the live object stays byte-identical until a single
//! atomic `CompleteMultipartUpload` swaps it with the new one.
//!
//! ## Atomicity (why no half-written object is ever visible)
//!
//! 1. `CreateMultipartUpload` allocates a *staging* upload. The
//!    live object at `s3://bucket/key.zip` is untouched. Readers
//!    hitting the archive mid-upload still get the old version.
//! 2. `UploadPartCopy` (prefix) + `UploadPart` (new tail) write
//!    into the staging area only.
//! 3. `CompleteMultipartUpload` is atomic server-side: either the
//!    assembled object replaces the live one, or nothing changes.
//!    `If-Match: <original-etag>` turns a lost-update race into a
//!    clean 412.
//! 4. On any error before step 3 we call `AbortMultipartUpload`;
//!    even if that call itself fails, a bucket lifecycle rule
//!    (`AbortIncompleteMultipartUpload`, standard setup) will GC
//!    the orphan.
//! 5. S3 may return HTTP 200 with `<Error>` in the body on
//!    Complete; we check the body, not just the status code (per
//!    issue #68).
//!
//! ## Scope (v1, matches issue #68 proposal)
//!
//! - Pure add only. `--delete`, `--sort`, `--atomic`, and name
//!   collisions with existing entries all error out.
//! - `s3://` only. Plain HTTP(S) targets stay read-only.
//! - Index flags (`--index-names`, `--index-terms`, `--facet`,
//!   `--write-columnar`) work as in local append — they only add
//!   new `.segN` entries at the tail.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use crate::walker::{self, WalkOpts, WorkItem};
use crate::options::Options;
use crate::progress::Progress;
use crate::sources::s3::S3Source;
use crate::sources::s3_write::{S3Target, CompletedPart, MAX_COPY_PART_SIZE, MIN_PART_SIZE};
use crate::zipreader::Archive;
use crate::zipwriter::ZipWriter;

/// How large an `UploadPart` body we send per round trip. 16 MiB is
/// a middle ground: large enough to keep SigV4 signing amortised
/// over real payload, small enough that memory for one in-flight
/// part stays reasonable. Separate from `MIN_PART_SIZE` (5 MiB)
/// which is S3's floor for non-last parts.
const UPLOAD_PART_SIZE: u64 = 16 * 1024 * 1024;

/// Did `opts.archive` point at an `s3://` URL? If so, update mode
/// takes the S3 append path instead of the local file path.
pub fn is_s3_archive(archive: &Path) -> bool {
    archive.to_string_lossy().starts_with("s3://")
}

/// Append-mode update on an `s3://` archive. Mirrors the shape of
/// `update::run_update_append` for the local case — the atomicity
/// story differs but the ZIP-level invariants are the same:
/// everything before the old CDR stays byte-identical, the new
/// CDR lists both old and new entries with their original
/// `lfh_offset`s, and AES envelopes on kept entries are
/// server-side copied without touching plaintext.
pub fn run_update_s3(opts: Options) -> Result<()> {
    let archive_url = opts.archive.to_string_lossy().to_string();
    let spec = archive_url
        .strip_prefix("s3://")
        .ok_or_else(|| anyhow!("not an s3 URL: {}", archive_url))?;

    // ── Scope enforcement (v1: pure add only) ─────────────────
    if !opts.delete.is_empty() {
        bail!(
            "tzip u on s3:// archives: --delete is not supported in v1 \
             (would require range-copy of every kept entry; proposed as follow-up)"
        );
    }
    if opts.atomic {
        bail!(
            "tzip u on s3:// archives: --atomic is implicit (CompleteMultipartUpload \
             is an atomic CAS) — drop the flag"
        );
    }
    if opts.sort {
        bail!(
            "tzip u on s3:// archives: --sort is not supported in v1 \
             (would require rebuilding over the kept bytes)"
        );
    }

    // ── 1. Open archive; capture ETag + cd_offset + tail ─────
    let mut source = S3Source::open(spec)
        .with_context(|| format!("open s3 archive {}", archive_url))?;
    let source_etag = source
        .fingerprint()
        .ok_or_else(|| {
            anyhow!(
                "s3 object {} returned no ETag; If-Match conditional writes won't work. \
                 MinIO / LocalStack with ETag stripped are not supported.",
                archive_url
            )
        })?
        .to_string();
    let object_url = source.url();

    // `Archive::open_source` consumes the source, so we reopen for
    // the tail snapshot. Alternative: open once, snapshot while we
    // still own it. Do the single-open form.
    let mut arch = Archive::open_source(Box::new(source))
        .with_context(|| format!("parse CDR of {}", archive_url))?;

    let cd_offset = arch.cd_offset();
    let orig_file_size = arch.file_size();

    // Collision check against existing names — pure-add scope
    // forbids replacing an existing entry.
    let old_names: HashSet<String> =
        arch.iter_entries().map(|e| e.name.clone()).collect();

    // Walk new input paths.
    let mut new_items: Vec<WorkItem> = if opts.paths.is_empty() {
        Vec::new()
    } else {
        walker::walk(WalkOpts {
            roots: &opts.paths,
            exclude: &opts.exclude,
            sort: false,
            walk_threads: opts.walk_jobs.max(1),
            basename_only: opts.basename_only,
        })?
    };
    // Deduplicate within new_items (first wins) to match
    // create-mode behaviour.
    {
        let mut seen: HashSet<String> = HashSet::new();
        new_items.retain(|it| seen.insert(it.name_in_archive.clone()));
    }
    for it in &new_items {
        if old_names.contains(&it.name_in_archive) {
            bail!(
                "tzip u on s3:// archives: {} already exists in the archive. \
                 Replace is not supported in v1 (pure-add only). Use a different \
                 name or delete+re-upload for replacements.",
                it.name_in_archive
            );
        }
    }

    if new_items.is_empty() {
        if !opts.quiet {
            eprintln!("tzip: nothing to do");
        }
        return Ok(());
    }

    // NB: unlike the local *rebuild* path (which needs
    // `lfh_total_size` to byte-copy LFH + body of each kept
    // entry), the S3 append path does NOT rewrite any kept
    // entry. It ships `[0, cd_offset)` through server-side
    // `UploadPartCopy` as a single opaque region, so the kept
    // LFHs never need to be enumerated client-side. We
    // deliberately skip the per-entry LFH probe loop that the
    // local rebuild uses — on a 100 K-entry archive that was
    // 100 K signed Range GETs of 30 bytes each (P1 audit).
    // `record_existing` below reads only CDR-sourced fields
    // (name, method, crc32, sizes, lfh_offset, mtime,
    // external_attr, stored digests) — none of which require
    // the LFH probe.

    // Snapshot existing entries so we can drop `arch` (freeing the
    // S3Source) before we start the upload. The tail bytes stay
    // referenced by their `lfh_offset`s — the upload will copy
    // them server-side from the live object.
    let existing: Vec<crate::zipreader::ReadEntry> = (0..arch.len())
        .map(|i| arch.entry(i).clone())
        .collect();
    // Phase 5b: resolve columnar schema before dropping arch.
    let columnar_schema = resolve_columnar_schema_for_s3(&opts, &mut arch)?;
    drop(arch);

    if !opts.quiet {
        eprintln!(
            "tzip: s3 append — object {} ({} bytes, etag {}), adding {} entries",
            archive_url,
            orig_file_size,
            source_etag,
            new_items.len()
        );
    }

    // ── 2. Encode new entries + rebuilt CDR to a temp file ───
    //
    // ZipWriter::new_at(temp, cd_offset) treats the temp file's
    // position 0 as *logical offset cd_offset*. Every new LFH and
    // the CDR we write carries absolute offsets that will be
    // correct once this temp file is appended after the kept
    // prefix in the final composed object.
    let mut temp = tempfile::NamedTempFile::new()
        .context("allocate temp file for s3 append tail")?;
    let tail_len = encode_new_tail(
        &opts,
        temp.as_file_mut(),
        &existing,
        cd_offset,
        new_items,
        columnar_schema,
    )?;
    temp.as_file_mut()
        .sync_all()
        .context("fsync temp append tail")?;

    // ── 3. Decide dispatch: multipart or single-PUT fallback ─
    //
    // Multipart's "all non-last parts ≥ 5 MiB" rule forces the
    // fallback when the kept prefix is tiny — we can't send a
    // sub-5-MiB `UploadPartCopy` as part 1, so we fetch the
    // prefix client-side and send one `PutObject` instead.
    let target = S3Target::open(spec)
        .with_context(|| format!("open s3 target for {}", archive_url))?;

    if cd_offset < MIN_PART_SIZE {
        single_put_fallback(
            &target,
            &object_url,
            &source_etag,
            cd_offset,
            spec,
            orig_file_size,
            temp.path(),
            tail_len,
            opts.quiet,
        )
    } else {
        multipart_append(
            &target,
            &object_url,
            &source_etag,
            cd_offset,
            temp.as_file_mut(),
            tail_len,
            opts.quiet,
        )
    }
}

/// Write the new LFH+body records and the rebuilt CDR for the
/// append into `temp`. Returns the length of what we wrote.
///
/// Mirrors the shape of `update::run_update_append`'s steps 3-4b,
/// but writes to a fresh temp file instead of truncating-and-
/// seeking the archive's own fd.
fn encode_new_tail(
    opts: &Options,
    temp: &mut File,
    existing: &[crate::zipreader::ReadEntry],
    cd_offset: u64,
    new_items: Vec<WorkItem>,
    columnar_schema: Option<crate::columnar::ColumnarSchema>,
) -> Result<u64> {
    use std::io::BufWriter;
    temp.seek(SeekFrom::Start(0))?;
    // `BufWriter` capacity: 1 MiB is plenty for the tail. We're
    // not optimising for bulk throughput here — the body work
    // happens in the parallel encode downstream.
    let writer = BufWriter::with_capacity(1024 * 1024, temp.try_clone()?);
    let mut zw = ZipWriter::new_at(writer, cd_offset);

    for re in existing {
        zw.record_existing(re, re.lfh_offset);
    }

    let total_new_bytes: u64 = new_items.iter().map(|i| i.size).sum();
    let total_new_files = new_items.len() as u64;
    let progress = Arc::new(Progress::new(
        total_new_bytes,
        total_new_files,
        opts.quiet || opts.tui,
    ));

    // Set up an IndexSegment just like local append so new
    // entries' names / terms / facets land in `.segN` sentinels.
    let base_idx = existing.len() as u32;
    let want_any_segment = opts.index_names
        || !opts.index_terms.is_empty()
        || !opts.facet.is_empty()
        || columnar_schema.is_some();
    let mut seg = if want_any_segment {
        Some(crate::update::IndexSegment::new(opts, base_idx, columnar_schema)?)
    } else {
        None
    };
    crate::update::encode_items_into_writer(
        opts,
        new_items,
        &mut zw,
        &progress,
        seg.as_mut(),
    )?;

    // Emit .segN sentinels for the newly added entries.
    if let Some(mut s) = seg.take() {
        let (name_seg_n, term_seg_n, facet_seg_ns, columnar_seg_n) =
            crate::update::next_segment_numbers(existing, &opts.facet);
        crate::update::write_index_segment_entries(
            &mut zw,
            &mut s,
            opts,
            name_seg_n,
            term_seg_n,
            &facet_seg_ns,
            columnar_seg_n,
        )?;
    }

    let mut inner = zw.finish()?;
    inner.flush()?;
    // `ZipWriter::new_at(..., cd_offset)` stamps absolute offsets
    // on LFH / CDR records assuming the first byte it writes
    // lands at the *logical* offset `cd_offset` in the final
    // composed object. On disk we're still writing into a temp
    // file that starts at position 0, so the tail's length is
    // whatever the *inner* BufWriter position is now — not the
    // logical ZipWriter position.
    let tail_len = inner.get_mut().stream_position()?;
    drop(inner);
    progress.finish("tail encoded");
    Ok(tail_len)
}

/// Phase 5b columnar-schema resolver shared with local update —
/// pull the schema off the existing base sidecar when the user
/// passed `--write-columnar` (bare) so new entries' columns
/// align with what's already stored.
fn resolve_columnar_schema_for_s3(
    opts: &Options,
    arch: &mut Archive,
) -> Result<Option<crate::columnar::ColumnarSchema>> {
    crate::update::resolve_columnar_schema_for_update(opts, arch)
}

/// Multipart append: server-side copy for `[0, cd_offset)` + new
/// upload for the tail + atomic Complete.
fn multipart_append(
    target: &S3Target,
    object_url: &str,
    source_etag: &str,
    cd_offset: u64,
    temp: &mut File,
    tail_len: u64,
    quiet: bool,
) -> Result<()> {
    let upload_id = target.create_multipart_upload()?;
    if !quiet {
        eprintln!("tzip: multipart upload {} started", upload_id);
    }

    // Guard the whole upload so any `?` bail propagates through
    // abort-on-error.
    let result = (|| -> Result<Vec<CompletedPart>> {
        let mut parts: Vec<CompletedPart> = Vec::new();
        let mut part_number: u32 = 1;

        // ── Phase A: UploadPartCopy over the kept prefix. ──
        //
        // Split `[0, cd_offset)` into ≤ MAX_COPY_PART_SIZE
        // chunks, each ≥ MIN_PART_SIZE (except possibly the
        // last, but here the last copy chunk always precedes
        // the Phase-B upload parts, so it is NOT the final
        // part and must still clear 5 MiB).
        //
        // For cd_offset < MIN_PART_SIZE the caller should have
        // dispatched to the single-PUT fallback already — we
        // assert that here.
        if cd_offset < MIN_PART_SIZE {
            bail!(
                "multipart_append called with cd_offset {} < MIN_PART_SIZE; \
                 use single_put_fallback",
                cd_offset
            );
        }
        let mut start: u64 = 0;
        while start < cd_offset {
            let remaining = cd_offset - start;
            let this_part = if remaining > MAX_COPY_PART_SIZE + MIN_PART_SIZE {
                MAX_COPY_PART_SIZE
            } else if remaining > MAX_COPY_PART_SIZE {
                // Would leave < MIN_PART_SIZE for the next
                // copy part, which S3 rejects. Split
                // evenly: half now, half next.
                remaining / 2
            } else {
                remaining
            };
            let end_inclusive = start + this_part - 1;
            if !quiet {
                eprintln!(
                    "tzip: upload-part-copy #{}  bytes {}-{} ({} MiB)",
                    part_number,
                    start,
                    end_inclusive,
                    this_part / (1024 * 1024)
                );
            }
            let etag = target.upload_part_copy(
                &upload_id,
                part_number,
                start,
                end_inclusive,
                source_etag,
            )?;
            parts.push(CompletedPart { part_number, etag });
            part_number += 1;
            start += this_part;
        }

        // ── Phase B: UploadPart over the new tail ──
        //
        // Split the tail into UPLOAD_PART_SIZE chunks. Every
        // non-last chunk must be ≥ MIN_PART_SIZE (S3 limit).
        // If the tail is smaller than MIN_PART_SIZE, we send
        // it as a single final part — the last part is exempt
        // from the 5 MiB floor.
        if tail_len == 0 {
            bail!("S3 append tail length is zero; no new entries encoded?");
        }
        let mut uploaded: u64 = 0;
        while uploaded < tail_len {
            let remaining = tail_len - uploaded;
            // If > 2 * UPLOAD_PART_SIZE left, send one chunk.
            // Otherwise send the rest as the final (possibly
            // small) part.
            let this_part = if remaining > 2 * UPLOAD_PART_SIZE {
                UPLOAD_PART_SIZE
            } else {
                remaining
            };
            if !quiet {
                eprintln!(
                    "tzip: upload-part     #{}  {} MiB (new tail)",
                    part_number,
                    (this_part + 1024 * 1024 - 1) / (1024 * 1024)
                );
            }
            let etag = target.upload_part_from_file(
                &upload_id,
                part_number,
                temp,
                uploaded,
                this_part,
            )?;
            parts.push(CompletedPart { part_number, etag });
            part_number += 1;
            uploaded += this_part;
        }

        Ok(parts)
    })();

    let parts = match result {
        Ok(p) => p,
        Err(e) => {
            if !quiet {
                eprintln!("tzip: upload failed, aborting multipart upload");
            }
            target.abort_multipart_upload(&upload_id);
            return Err(e.context("s3 multipart upload"));
        }
    };

    // ── Phase C: atomic CompleteMultipartUpload ──
    //
    // Pass If-Match: <original-etag> so a concurrent writer that
    // replaced the object between our open and this call fails
    // this with 412 (no partial writes land, no data lost).
    // Custom endpoints (MinIO / LocalStack) may ignore If-Match;
    // callers who need the CAS must run against real S3 or a
    // backend that honours it.
    let if_match = if target.uses_custom_endpoint() {
        None
    } else {
        Some(source_etag)
    };
    let new_etag = target
        .complete_multipart_upload(&upload_id, &parts, if_match)
        .map_err(|e| {
            // Complete itself failed. S3 tells us the state is
            // unchanged — the live object is still the original.
            // No abort needed; the MPU's parts stay orphaned
            // and the bucket's lifecycle rule GCs them.
            e.context("CompleteMultipartUpload")
        })?;

    if !quiet {
        eprintln!(
            "tzip: s3 append complete — new etag {} for {}",
            new_etag, object_url
        );
    }
    Ok(())
}

/// Small-archive fallback: when the kept prefix is below
/// `MIN_PART_SIZE`, multipart's "all non-last parts ≥ 5 MiB" rule
/// forbids `UploadPartCopy` from carrying it alone. Fetch the
/// prefix via `GET`, concat with the new tail, send one
/// `PutObject` with `If-Match` for the same CAS semantics.
fn single_put_fallback(
    target: &S3Target,
    object_url: &str,
    source_etag: &str,
    cd_offset: u64,
    spec: &str,
    orig_file_size: u64,
    temp_path: &Path,
    tail_len: u64,
    quiet: bool,
) -> Result<()> {
    if !quiet {
        eprintln!(
            "tzip: cd_offset {} < 5 MiB — using single-PUT fallback",
            cd_offset
        );
    }
    // Re-open the source for a single Range GET; cheaper than
    // threading the original S3Source through every function.
    let mut src = S3Source::open_with_len(spec, orig_file_size)
        .with_context(|| format!("re-open s3 source for prefix fetch of {}", object_url))?;
    let mut prefix = vec![0u8; cd_offset as usize];
    src.seek(SeekFrom::Start(0))
        .context("seek s3 source to 0")?;
    src.read_exact(&mut prefix)
        .context("read prefix bytes from s3 source")?;
    // Concatenate prefix + temp tail into a single buffer. For
    // the fallback path the total is < MIN_PART_SIZE + tail,
    // bounded by whatever the user is appending; if the resulting
    // object exceeds ~several GB the user should use multipart
    // anyway (which kicks in automatically at cd_offset ≥ 5 MiB).
    let mut buf = prefix;
    buf.reserve(tail_len as usize);
    let mut temp_file = File::open(temp_path).context("reopen temp tail for PUT")?;
    let mut tail = Vec::with_capacity(tail_len as usize);
    temp_file
        .read_to_end(&mut tail)
        .context("read temp tail bytes")?;
    buf.extend_from_slice(&tail);

    let if_match = if target.uses_custom_endpoint() {
        None
    } else {
        Some(source_etag)
    };
    let new_etag = target.put_object_if_match(&buf, if_match)?;
    if !quiet {
        eprintln!(
            "tzip: s3 append complete — new etag {} for {} ({} bytes)",
            new_etag,
            object_url,
            buf.len()
        );
    }
    Ok(())
}
