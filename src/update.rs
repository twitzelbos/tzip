//! Update mode: add, replace, or delete entries in an existing ZIP archive
//! without re-reading / re-compressing / re-encrypting the kept entries.
//!
//! Two paths:
//!
//! - **Append mode** (near-instant, the "truly no regeneration" path).
//!   The archive is updated in place: truncate at the old central-directory
//!   offset, append new LFH+body records, write a fresh CDR covering old
//!   entries (at their original offsets) plus the new ones. An on-disk
//!   **undo file** (`<archive>.tzip-undo`) holds the old CDR tail bytes so
//!   a crash mid-write is recoverable on the next launch. Kicks in when
//!   the update is pure-add: no `--delete`, no name collision with an
//!   existing entry, not `--atomic`, not `--sort`.
//!
//! - **Rebuild mode** (correct in all cases). Write a fresh archive to
//!   `<archive>.tzip-tmp-<pid>`, byte-copy the kept entries' LFH + body
//!   (no decrypt, no decompress), encode new entries through the normal
//!   parallel pipeline, `sync_all`, atomic-rename over the original.
//!
//! Both paths preserve WinZIP AES-256 envelopes on kept entries by copying
//! the raw body bytes.

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::bounded;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;

use crate::cli::{Method, Options};
use crate::compress::{self, Scratch};
use crate::pipeline::{self, LocalFsSource, RawItem, Source};
use crate::platform;
use crate::progress::Progress;
use crate::walker::{self, WalkOpts, WorkItem};
use crate::zipreader::{Archive, ReadEntry};
use crate::zipwriter::{EncodedEntry, ZipWriter};

/// Undo file magic. Lets us recognise an on-disk undo file as ours and
/// refuse to act on anything else.
const UNDO_MAGIC: &[u8; 8] = b"TZIPUNDO";
const UNDO_VERSION: u32 = 1;

pub fn run_update(opts: Options) -> Result<()> {
    // Recover from a prior interrupted append-mode update, if any.
    attempt_recover_undo(&opts.archive)?;

    // Try to open the archive rw so the append-mode branch (below)
    // can reuse the handle. Multi-disk and read-only inputs fall back
    // to `Archive::open` + rebuild mode — `rw_handle` goes `None` on
    // either and we force `can_append = false` to match.
    let (mut arch, rw_handle): (Archive, Option<std::fs::File>) =
        match Archive::open_rw(&opts.archive) {
            Ok((a, f)) => (a, Some(f)),
            Err(_) => {
                let a = Archive::open(&opts.archive).with_context(|| {
                    format!("open archive {} for update", opts.archive.display())
                })?;
                (a, None)
            }
        };

    // Walk new input paths (batch; we need the full list for collision
    // detection and sort-mode ordering).
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

    // Compute delete + replace sets.
    let delete_matches: HashSet<String> = arch
        .entries()
        .iter()
        .filter(|e| walker::is_excluded(&e.name, &opts.delete))
        .map(|e| e.name.clone())
        .collect();
    let new_names: HashSet<String> = new_items
        .iter()
        .map(|it| it.name_in_archive.clone())
        .collect();
    let old_names: HashSet<String> = arch.entries().iter().map(|e| e.name.clone()).collect();
    let has_replace = new_names.iter().any(|n| old_names.contains(n));

    // Deduplicate within new_items (first wins) to match create-mode behaviour.
    {
        let mut seen: HashSet<String> = HashSet::new();
        new_items.retain(|it| seen.insert(it.name_in_archive.clone()));
    }

    // Populate LFH sizes on the Archive's own entries up front — one
    // linear pass. The previous flow cloned first and then did a
    // name-based `.position()` per kept entry at carry time, which
    // was O(N²) over the archive on large sets. Now the cloned
    // entries in `kept` already carry `lfh_total_size`, so
    // `populate_lfh_size` collapses to a no-op.
    let mut kept: Vec<ReadEntry> = Vec::new();
    for idx in 0..arch.entries().len() {
        let name = arch.entries()[idx].name.clone();
        if delete_matches.contains(&name) || new_names.contains(&name) {
            continue;
        }
        arch.lfh_total_size_of(idx)
            .with_context(|| format!("probe LFH for carried entry {}", name))?;
        kept.push(arch.entries()[idx].clone());
    }

    if new_items.is_empty() && delete_matches.is_empty() {
        if !opts.quiet {
            eprintln!("tzip: nothing to do (no new files, nothing matched --delete)");
        }
        return Ok(());
    }

    let can_append = !opts.atomic
        && opts.delete.is_empty()
        && !has_replace
        && !opts.sort
        && rw_handle.is_some();

    if !opts.quiet {
        let mode = if can_append { "append" } else { "rebuild" };
        let n_add = new_items.len();
        let n_del = delete_matches.len();
        let n_rep = new_names.iter().filter(|n| old_names.contains(*n)).count();
        eprintln!(
            "tzip: update mode = {} (add {}, replace {}, delete {}, keep {})",
            mode,
            n_add.saturating_sub(n_rep),
            n_rep,
            n_del,
            kept.len(),
        );
    }

    if can_append {
        run_update_append(&opts, arch, new_items, rw_handle.unwrap())
    } else {
        run_update_rebuild(&opts, arch, kept, new_items)
    }
}

// ── Append mode ─────────────────────────────────────────────────────────────

fn run_update_append(
    opts: &Options,
    mut arch: Archive,
    new_items: Vec<WorkItem>,
    mut file: std::fs::File,
) -> Result<()> {
    let undo_path = undo_path_for(&opts.archive);

    // 1. Write the undo file (exclusive-create so a concurrent `-u` run fails
    //    cleanly here instead of corrupting state).
    let orig_file_size = arch.file_size();
    let cd_offset = arch.cd_offset();
    let tail = arch.tail_bytes().context("snapshot archive tail for undo")?;
    drop(arch);
    write_undo_file(&undo_path, orig_file_size, cd_offset, &tail)
        .with_context(|| format!("write undo file {}", undo_path.display()))?;

    // 2. Truncate the archive at cd_offset on the rw fd we were handed
    //    (same open as the one Archive parsed from, dup'd at open time).
    file.set_len(cd_offset)
        .context("truncate archive at cd_offset")?;
    file.seek(SeekFrom::Start(cd_offset))
        .context("seek to cd_offset")?;

    // 3. BufWriter + ZipWriter positioned at cd_offset. Rebuild the central
    //    directory from the entries we parsed. fstatfs on the rw fd we
    //    already hold — no second path walk.
    #[cfg(unix)]
    let out_info = {
        use std::os::unix::io::AsRawFd;
        platform::fs_info_fd(file.as_raw_fd()).ok()
    };
    #[cfg(not(unix))]
    let out_info = platform::fs_info(
        opts.archive
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new(".")),
    )
    .ok();
    let cap = pipeline::writer_capacity_for(out_info.as_ref());
    let writer = BufWriter::with_capacity(cap, file);
    let mut zw = ZipWriter::new_at(writer, cd_offset);

    // Re-open a read-only handle solely to parse the old CDR we snapshotted.
    // We already have the parsed entries in `tail`; parse them back out so
    // the writer's central directory lists them with their *original*
    // lfh_offsets (unchanged on disk).
    let existing = reparse_entries_from_tail(&tail, cd_offset)?;
    for re in &existing {
        zw.record_existing(re, re.lfh_offset);
    }

    // 4. Run the reader/CPU pool on the new items; writes land at the end
    //    of the appended region.
    let total_new_bytes: u64 = new_items.iter().map(|i| i.size).sum();
    let total_new_files = new_items.len() as u64;
    let progress = Arc::new(Progress::new(
        total_new_bytes,
        total_new_files,
        opts.quiet || opts.tui,
    ));
    let result = encode_items_into_writer(opts, new_items, &mut zw, &progress);

    if let Err(e) = result {
        // Try to restore via undo. If restore fails, leave the undo file in
        // place so the next tzip -u can retry.
        let _ = restore_from_undo(&opts.archive, &undo_path);
        return Err(e.context("append-mode update failed"));
    }

    // 5. Finish + fsync + delete undo. Order matters — we must know the
    //    archive is durable on disk *before* we remove the undo file.
    let mut inner = zw.finish()?;
    inner.flush()?;
    inner.get_mut().sync_all().context("sync_all on archive")?;
    drop(inner);

    std::fs::remove_file(&undo_path).with_context(|| {
        format!(
            "remove undo file {} after successful update",
            undo_path.display()
        )
    })?;
    progress.finish("done");
    Ok(())
}

// ── Rebuild mode ───────────────────────────────────────────────────────────

fn run_update_rebuild(
    opts: &Options,
    mut arch: Archive,
    kept: Vec<ReadEntry>,
    new_items: Vec<WorkItem>,
) -> Result<()> {
    let pid = std::process::id();
    let tmp_name = format!(
        "{}.tzip-tmp-{}",
        opts.archive
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive.zip"),
        pid,
    );
    let tmp_path: PathBuf = opts
        .archive
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(&tmp_name))
        .unwrap_or_else(|| PathBuf::from(&tmp_name));

    let result = (|| -> Result<()> {
        let file = File::create(&tmp_path)
            .with_context(|| format!("create temp archive {}", tmp_path.display()))?;
        // fstatfs on the fd we just opened — skips the path walk a
        // path-based fs_info(parent) would do.
        #[cfg(unix)]
        let out_info = {
            use std::os::unix::io::AsRawFd;
            platform::fs_info_fd(file.as_raw_fd()).ok()
        };
        #[cfg(not(unix))]
        let out_info = platform::fs_info(
            opts.archive
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new(".")),
        )
        .ok();
        // Rough upper bound: carried bytes + 1.05× new bytes. Used only as
        // a preallocation hint; ftruncate on finish trims unused tail.
        let kept_bytes: u64 = kept.iter().map(|e| e.comp_size).sum();
        let new_bytes: u64 = new_items.iter().map(|i| i.size).sum();
        let prealloc = kept_bytes + (new_bytes as f64 * 1.05) as u64 + 64 * 1024;
        let _ = platform::preallocate(&file, prealloc);

        let cap = pipeline::writer_capacity_for(out_info.as_ref());
        let writer = BufWriter::with_capacity(cap, file);
        let mut zw = ZipWriter::new_at(writer, 0);

        let progress = Arc::new(Progress::new(
            kept_bytes + new_bytes,
            kept.len() as u64 + new_items.len() as u64,
            opts.quiet || opts.tui,
        ));

        if opts.sort {
            // Build a sorted merged list, pre-encode all new items in
            // parallel into a HashMap, then walk the sorted list and
            // dispatch each slot to either byte-copy or pre-encoded write.
            let mut mixed: Vec<Mixed> = kept
                .into_iter()
                .map(Mixed::Kept)
                .chain(new_items.into_iter().map(Mixed::New))
                .collect();
            mixed.sort_by(|a, b| a.name().cmp(b.name()));

            let new_positions: Vec<(usize, WorkItem)> = mixed
                .iter()
                .enumerate()
                .filter_map(|(i, m)| match m {
                    Mixed::New(w) => Some((i, w.clone())),
                    _ => None,
                })
                .collect();
            let mut encoded = encode_items_to_map(opts, new_positions, &progress)?;

            for (i, m) in mixed.iter().enumerate() {
                match m {
                    Mixed::Kept(re) => {
                        let mut re = re.clone();
                        let name = re.name.clone();
                        populate_lfh_size(&mut arch, &mut re, name)?;
                        zw.carry_existing_entry(arch.source_mut(), &re)?;
                        progress.inc(re.comp_size);
                        progress.set_msg(format!("carried {}", re.name));
                    }
                    Mixed::New(_) => {
                        let e = encoded
                            .remove(&i)
                            .ok_or_else(|| anyhow!("missing encoded entry at index {}", i))?;
                        let plain = e.uncompressed_size;
                        let msg = format!("added {}", e.name);
                        zw.write_entry(e)?;
                        progress.inc(plain);
                        progress.set_msg(msg);
                    }
                }
            }
        } else {
            // Phase 1: byte-copy kept entries in original CDR order.
            for re in &kept {
                let mut re = re.clone();
                let name = re.name.clone();
                populate_lfh_size(&mut arch, &mut re, name)?;
                zw.carry_existing_entry(arch.source_mut(), &re)?;
                progress.inc(re.comp_size);
                progress.set_msg(format!("carried {}", re.name));
            }
            // Phase 2: parallel encode of new items.
            encode_items_into_writer(opts, new_items, &mut zw, &progress)?;
        }

        let mut inner = zw.finish()?;
        inner.flush()?;
        inner.get_mut().sync_all().context("sync_all on temp archive")?;
        // Trim preallocated tail to the actual archive size.
        let actual_len = inner.get_mut().stream_position().unwrap_or(0);
        if actual_len > 0 {
            let _ = inner.get_mut().set_len(actual_len);
        }
        drop(inner);
        progress.finish("done");
        Ok(())
    })();

    match result {
        Ok(()) => std::fs::rename(&tmp_path, &opts.archive).with_context(|| {
            format!(
                "atomic rename {} -> {}",
                tmp_path.display(),
                opts.archive.display()
            )
        }),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

enum Mixed {
    Kept(ReadEntry),
    New(WorkItem),
}

impl Mixed {
    fn name(&self) -> &str {
        match self {
            Mixed::Kept(re) => &re.name,
            Mixed::New(w) => &w.name_in_archive,
        }
    }
}

fn populate_lfh_size(arch: &mut Archive, re: &mut ReadEntry, want_name: String) -> Result<()> {
    if re.lfh_total_size != 0 {
        return Ok(());
    }
    // Find the entry index by matching name (cheap: entries aren't huge).
    let idx = arch
        .entries()
        .iter()
        .position(|e| e.name == want_name)
        .ok_or_else(|| anyhow!("internal: entry {} vanished from archive", want_name))?;
    re.lfh_total_size = arch.lfh_total_size_of(idx)?;
    Ok(())
}

// ── Encoder helpers ────────────────────────────────────────────────────────

fn encode_items_into_writer<W: Write + Seek>(
    opts: &Options,
    items: Vec<WorkItem>,
    zw: &mut ZipWriter<W>,
    progress: &Arc<Progress>,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let total_bytes: u64 = items.iter().map(|i| i.size).sum();
    let total_files = items.len() as u64;

    let read_bound = (opts.read_jobs * 2).max(2);
    let cpu_bound = (opts.cpu_jobs * 2).max(2);
    let (raw_tx, raw_rx) = bounded::<RawItem>(read_bound);
    let (enc_tx, enc_rx) = bounded::<EncodedEntry>(cpu_bound);
    let (feed_tx, feed_rx) = bounded::<(u64, WorkItem)>(read_bound);

    let index_counter = Arc::new(AtomicU64::new(0));
    let feeder = {
        let feed_tx = feed_tx.clone();
        let index_counter = Arc::clone(&index_counter);
        thread::spawn(move || -> Result<()> {
            for it in items {
                let idx = index_counter.fetch_add(1, AtomicOrdering::Relaxed);
                if feed_tx.send((idx, it)).is_err() {
                    break;
                }
            }
            Ok(())
        })
    };
    drop(feed_tx);

    let source: Arc<dyn Source> = Arc::new(LocalFsSource { keep_cache: opts.keep_cache });
    let mut reader_handles = Vec::new();
    for _ in 0..opts.read_jobs {
        let feed_rx = feed_rx.clone();
        let raw_tx = raw_tx.clone();
        let source = Arc::clone(&source);
        reader_handles.push(thread::spawn(move || -> Result<()> {
            while let Ok((index, item)) = feed_rx.recv() {
                let bytes = source.read(&item)?;
                if raw_tx
                    .send(RawItem { index, item, bytes })
                    .is_err()
                {
                    break;
                }
            }
            Ok(())
        }));
    }
    drop(feed_rx);
    drop(raw_tx);

    let method = opts.method;
    let level = opts.level;
    let password = opts.password.clone();
    let single_file_parallel = matches!(method, Method::Deflate)
        && total_files == 1
        && total_bytes >= compress::PARALLEL_DEFLATE_THRESHOLD;

    let mut cpu_handles = Vec::new();
    for _ in 0..opts.cpu_jobs {
        let raw_rx = raw_rx.clone();
        let enc_tx = enc_tx.clone();
        let password = password.clone();
        cpu_handles.push(thread::spawn(move || -> Result<()> {
            let mut scratch = Scratch::new();
            while let Ok(raw) = raw_rx.recv() {
                let entry = pipeline::encode_item(
                    &raw,
                    method,
                    level,
                    password.as_deref(),
                    &mut scratch,
                    single_file_parallel,
                )?;
                if enc_tx.send(entry).is_err() {
                    break;
                }
            }
            Ok(())
        }));
    }
    drop(raw_rx);
    drop(enc_tx);

    let mut wrote = 0u64;
    while let Ok(entry) = enc_rx.recv() {
        wrote += 1;
        let plain = entry.uncompressed_size;
        // Build the strings that borrow `entry.name` BEFORE handing
        // ownership to `write_entry` — avoids a `name.clone()` per entry.
        let msg = format!("{}/{} — {}", wrote, total_files, entry.name);
        if opts.verbose {
            eprintln!("added {}", entry.name);
        }
        zw.write_entry(entry)?;
        progress.inc(plain);
        progress.set_msg(msg);
    }

    feeder
        .join()
        .map_err(|_| anyhow!("feeder thread panicked"))??;
    for h in reader_handles {
        h.join().ok().transpose()?;
    }
    for h in cpu_handles {
        h.join().ok().transpose()?;
    }
    Ok(())
}

/// Encode items in parallel, but return a `HashMap<position, EncodedEntry>`
/// keyed by the caller-supplied slot index instead of writing anywhere.
/// Used by rebuild + --sort mode where kept + new must interleave in a
/// deterministic order.
fn encode_items_to_map(
    opts: &Options,
    positioned_items: Vec<(usize, WorkItem)>,
    progress: &Arc<Progress>,
) -> Result<HashMap<usize, EncodedEntry>> {
    let mut map: HashMap<usize, EncodedEntry> = HashMap::new();
    if positioned_items.is_empty() {
        return Ok(map);
    }

    let positions: HashMap<u64, usize> = positioned_items
        .iter()
        .enumerate()
        .map(|(i, (pos, _))| (i as u64, *pos))
        .collect();
    let items: Vec<WorkItem> = positioned_items.into_iter().map(|(_, w)| w).collect();
    let total_files = items.len() as u64;
    let total_bytes: u64 = items.iter().map(|i| i.size).sum();

    let read_bound = (opts.read_jobs * 2).max(2);
    let cpu_bound = (opts.cpu_jobs * 2).max(2);
    let (raw_tx, raw_rx) = bounded::<RawItem>(read_bound);
    let (enc_tx, enc_rx) = bounded::<EncodedEntry>(cpu_bound);
    let (feed_tx, feed_rx) = bounded::<(u64, WorkItem)>(read_bound);

    let feeder = {
        let feed_tx = feed_tx.clone();
        thread::spawn(move || -> Result<()> {
            for (i, it) in items.into_iter().enumerate() {
                if feed_tx.send((i as u64, it)).is_err() {
                    break;
                }
            }
            Ok(())
        })
    };
    drop(feed_tx);

    let source: Arc<dyn Source> = Arc::new(LocalFsSource { keep_cache: opts.keep_cache });
    let mut reader_handles = Vec::new();
    for _ in 0..opts.read_jobs {
        let feed_rx = feed_rx.clone();
        let raw_tx = raw_tx.clone();
        let source = Arc::clone(&source);
        reader_handles.push(thread::spawn(move || -> Result<()> {
            while let Ok((index, item)) = feed_rx.recv() {
                let bytes = source.read(&item)?;
                if raw_tx
                    .send(RawItem { index, item, bytes })
                    .is_err()
                {
                    break;
                }
            }
            Ok(())
        }));
    }
    drop(feed_rx);
    drop(raw_tx);

    let method = opts.method;
    let level = opts.level;
    let password = opts.password.clone();
    let single_file_parallel = matches!(method, Method::Deflate)
        && total_files == 1
        && total_bytes >= compress::PARALLEL_DEFLATE_THRESHOLD;

    let mut cpu_handles = Vec::new();
    for _ in 0..opts.cpu_jobs {
        let raw_rx = raw_rx.clone();
        let enc_tx = enc_tx.clone();
        let password = password.clone();
        cpu_handles.push(thread::spawn(move || -> Result<()> {
            let mut scratch = Scratch::new();
            while let Ok(raw) = raw_rx.recv() {
                let entry = pipeline::encode_item(
                    &raw,
                    method,
                    level,
                    password.as_deref(),
                    &mut scratch,
                    single_file_parallel,
                )?;
                if enc_tx.send(entry).is_err() {
                    break;
                }
            }
            Ok(())
        }));
    }
    drop(raw_rx);
    drop(enc_tx);

    while let Ok(entry) = enc_rx.recv() {
        let pos = positions
            .get(&entry.index)
            .copied()
            .ok_or_else(|| anyhow!("unknown entry index {} from encoder", entry.index))?;
        // Progress is advanced later at the writer stage so sorted output
        // and progress counters stay in sync.
        let _ = progress;
        map.insert(pos, entry);
    }

    feeder
        .join()
        .map_err(|_| anyhow!("feeder thread panicked"))??;
    for h in reader_handles {
        h.join().ok().transpose()?;
    }
    for h in cpu_handles {
        h.join().ok().transpose()?;
    }
    Ok(map)
}

// ── Undo file ──────────────────────────────────────────────────────────────

fn undo_path_for(archive: &Path) -> PathBuf {
    let mut s = archive.as_os_str().to_os_string();
    s.push(".tzip-undo");
    PathBuf::from(s)
}

fn write_undo_file(
    path: &Path,
    orig_file_size: u64,
    cd_offset: u64,
    tail: &[u8],
) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "create undo file {} (another tzip -u may be running, or a \
                 prior run was interrupted — remove it manually if you're sure)",
                path.display()
            )
        })?;
    f.write_all(UNDO_MAGIC)?;
    f.write_all(&UNDO_VERSION.to_le_bytes())?;
    f.write_all(&orig_file_size.to_le_bytes())?;
    f.write_all(&cd_offset.to_le_bytes())?;
    f.write_all(tail)?;
    f.sync_all()?;
    Ok(())
}

struct UndoHeader {
    orig_file_size: u64,
    cd_offset: u64,
    tail_offset: u64,
}

fn read_undo_header(path: &Path) -> Result<UndoHeader> {
    let mut f = File::open(path).with_context(|| format!("open undo {}", path.display()))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).context("read undo magic")?;
    if &magic != UNDO_MAGIC {
        bail!("{} is not a tzip undo file", path.display());
    }
    let mut v = [0u8; 4];
    f.read_exact(&mut v)?;
    let version = u32::from_le_bytes(v);
    if version != UNDO_VERSION {
        bail!(
            "undo file {} has version {} (expected {})",
            path.display(),
            version,
            UNDO_VERSION
        );
    }
    let mut o = [0u8; 8];
    f.read_exact(&mut o)?;
    let orig_file_size = u64::from_le_bytes(o);
    f.read_exact(&mut o)?;
    let cd_offset = u64::from_le_bytes(o);
    let tail_offset = f.stream_position()?;
    Ok(UndoHeader { orig_file_size, cd_offset, tail_offset })
}

pub fn attempt_recover_undo(archive: &Path) -> Result<()> {
    let undo = undo_path_for(archive);
    if !undo.exists() {
        return Ok(());
    }
    let hdr = read_undo_header(&undo)
        .with_context(|| format!("parse undo {}", undo.display()))?;
    let current_size = match std::fs::metadata(archive) {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if current_size == hdr.orig_file_size {
        // No in-flight crash — the archive is intact and matches what the
        // undo file would restore it to. Stale undo, silently drop.
        std::fs::remove_file(&undo)
            .with_context(|| format!("remove stale undo {}", undo.display()))?;
        return Ok(());
    }
    // Crash recovery: restore the tail. Hand the header down so we
    // don't open+reparse the undo file a second time.
    restore_from_undo_with_hdr(archive, &undo, &hdr)?;
    eprintln!(
        "tzip: recovered {} from interrupted update (undo size = {} bytes)",
        archive.display(),
        hdr.orig_file_size
    );
    Ok(())
}

/// Legacy entry: used by the in-process error recovery path in
/// `run_update_append`, which doesn't have an `UndoHeader` in scope.
/// Reads the header, then delegates.
fn restore_from_undo(archive: &Path, undo: &Path) -> Result<()> {
    let hdr = read_undo_header(undo)?;
    restore_from_undo_with_hdr(archive, undo, &hdr)
}

fn restore_from_undo_with_hdr(archive: &Path, undo: &Path, hdr: &UndoHeader) -> Result<()> {
    let mut undo_file = File::open(undo)?;
    undo_file
        .seek(SeekFrom::Start(hdr.tail_offset))
        .context("seek to undo tail")?;
    let mut tail = Vec::new();
    undo_file.read_to_end(&mut tail).context("read undo tail")?;

    let mut archive_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(archive)
        .with_context(|| format!("reopen {} for restore", archive.display()))?;
    archive_file
        .set_len(hdr.cd_offset)
        .context("truncate archive to cd_offset")?;
    archive_file
        .seek(SeekFrom::Start(hdr.cd_offset))
        .context("seek for tail restore")?;
    archive_file
        .write_all(&tail)
        .context("write tail back")?;
    archive_file.sync_all().context("sync after restore")?;
    drop(archive_file);
    std::fs::remove_file(undo)
        .with_context(|| format!("remove undo {} after restore", undo.display()))?;
    Ok(())
}

/// Parse the central-directory bytes we snapshotted for the undo file,
/// so we can reconstruct the existing-entries list without reopening
/// the archive. Shares the CDR parser with `zipreader`, directly from
/// the in-memory tail — no scratch file, no fsync, no second open.
fn reparse_entries_from_tail(tail: &[u8], _cd_offset: u64) -> Result<Vec<ReadEntry>> {
    crate::zipreader::parse_entries_from_tail(tail)
}

