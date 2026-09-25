//! Read → compress+encrypt → write, with the read side deliberately capped
//! to a small pool so external drives don't get seek-stormed.
//!
//! The read side lives behind a `Source` trait so a future backend that
//! streams from S3, Google Drive, or Box can plug in without changing the
//! pipeline. For now the only implementation is `LocalFsSource`.

use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;
use std::thread;

use crate::cli::{Method, Options};
use crate::compress::{self, Scratch};
use crate::crypto;
use crate::platform;
use crate::progress::Progress;
use crate::sevenz;
use crate::tui;
use crate::walker::{self, WalkOpts, WorkItem};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use crate::zipwriter::{EncodedEntry, ZipWriter, METHOD_AES};

/// Abstract "give me the bytes for this item" — the seam where cloud
/// backends will plug in later. Trait objects live in an Arc so multiple
/// reader threads can share one instance without cloning per-item state.
pub trait Source: Send + Sync {
    fn read(&self, item: &WorkItem) -> Result<platform::ReadBuf>;
}

pub struct LocalFsSource {
    /// If true, keep read blocks in the OS page cache (default: bypass).
    pub keep_cache: bool,
}

impl Source for LocalFsSource {
    fn read(&self, item: &WorkItem) -> Result<platform::ReadBuf> {
        // Fast path: openat(dirfd, basename) when the walker gave us a dirfd.
        // This skips per-component path resolution — a real win on the deep
        // directory layouts we see in DICOM trees.
        if let (Some(dirfd), Some(basename)) = (&item.dirfd, &item.basename) {
            match platform::open_at(dirfd.as_raw(), basename) {
                Ok(f) => {
                    return platform::read_from_file(f, item.size, self.keep_cache)
                        .with_context(|| format!("read {}", item.path.display()));
                }
                Err(e) => {
                    // Fall through to path-based open, e.g. if the dirfd was
                    // closed by the OS or the file was renamed.
                    if self.keep_cache {
                        eprintln!(
                            "openat failed for {}: {} — falling back to open by path",
                            item.path.display(),
                            e
                        );
                    }
                }
            }
        }
        platform::read_input(&item.path, item.size, self.keep_cache)
            .with_context(|| format!("read {}", item.path.display()))
    }
}

/// Read stage output — untouched bytes plus enough metadata to identify the
/// entry downstream.
struct RawItem {
    index: u64,
    item: WorkItem,
    bytes: platform::ReadBuf,
}

pub fn run(opts: Options) -> Result<()> {
    // Optional USB / drive diagnostic — only prints if the workload actually
    // touches a USB-backed filesystem. Cheap when everything is internal.
    #[cfg(target_os = "macos")]
    {
        if opts.usb_info {
            let reports = crate::usb_info::probe_workload(&opts.paths, &opts.archive);
            crate::usb_info::print_reports(&reports, opts.read_jobs, opts.keep_cache);
        }
        if opts.warn_contention {
            // Collect distinct mount points for every source + output
            let mut mounts: Vec<std::path::PathBuf> = Vec::new();
            for p in &opts.paths {
                if let Ok(info) = platform::fs_info(p) {
                    if !mounts.contains(&info.mount_point) {
                        mounts.push(info.mount_point);
                    }
                }
            }
            let out_parent = opts
                .archive
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            if let Ok(info) = platform::fs_info(&out_parent) {
                if !mounts.contains(&info.mount_point) {
                    mounts.push(info.mount_point);
                }
            }
            let report = crate::contention::probe(&mounts);
            crate::contention::print_warnings(&report);
        }
    }

    // Auto-tune defaults based on source/output filesystem. User-set flags
    // are never overridden.
    let opts = auto_tune(opts);

    // .7z solid needs the full list up front (it's serial LZMA2 and the
    // sevenz-rust API takes an ordered iterator). Use the batch walk.
    if opts.solid {
        let items = walker::walk(WalkOpts {
            roots: &opts.paths,
            exclude: &opts.exclude,
            sort: opts.sort,
            walk_threads: opts.walk_jobs.max(1),
        })?;
        if items.is_empty() {
            anyhow::bail!("no files to archive");
        }
        let total_bytes: u64 = items.iter().map(|i| i.size).sum();
        let total_files = items.len() as u64;
        return sevenz::write_solid(&opts, &items, total_bytes, total_files);
    }

    // `--sort` also uses the batch walk — we need the full list to reorder
    // arrivals into deterministic order.
    let batch_items: Option<Vec<WorkItem>> = if opts.sort {
        let items = walker::walk(WalkOpts {
            roots: &opts.paths,
            exclude: &opts.exclude,
            sort: opts.sort,
            walk_threads: opts.walk_jobs.max(1),
        })?;
        if items.is_empty() {
            anyhow::bail!("no files to archive");
        }
        Some(items)
    } else {
        None
    };

    // In streaming mode we don't know the total until the walker finishes.
    // Show an indeterminate spinner + running byte counter; flip to a gauge
    // once we know the total.
    let (total_bytes_hint, total_files_hint): (u64, u64) = match &batch_items {
        Some(v) => (v.iter().map(|i| i.size).sum(), v.len() as u64),
        None => (0, 0),
    };

    let progress = Arc::new(Progress::new(
        total_bytes_hint,
        total_files_hint,
        opts.quiet && !opts.tui,
    ));
    let tui_handle = if opts.tui {
        Some(tui::start(total_bytes_hint, total_files_hint, opts.cpu_jobs))
    } else {
        None
    };

    // 2. Open archive + best-effort preallocation + FS-aware buffer sizing
    let file = File::create(&opts.archive)
        .with_context(|| format!("create archive {}", opts.archive.display()))?;
    let out_info = platform::fs_info(
        opts.archive
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new(".")),
    )
    .ok();

    // Preallocate extents if we know an upper-bound size. Upper-bound for
    // any compression method is `uncompressed_total × 1.05` (AES adds
    // ~28 bytes/entry + CDR overhead). Streaming mode with unknown total
    // preallocates a modest 256 MiB default.
    let preallocate_len: u64 = if total_bytes_hint > 0 {
        (total_bytes_hint as f64 * 1.05).round() as u64
    } else {
        256 * 1024 * 1024
    };
    let _ = platform::preallocate(&file, preallocate_len);

    // BufWriter capacity — bump on exFAT/msdos (large clusters, sequential
    // writes benefit from big flushes) and scale to iosize otherwise.
    let cap = choose_writer_capacity(out_info.as_ref());
    let writer = BufWriter::with_capacity(cap, file);
    let mut zw = ZipWriter::new(writer);

    // 3. Channels: feed → read → cpu → writer
    let read_bound = (opts.read_jobs * 2).max(2);
    let cpu_bound = (opts.cpu_jobs * 2).max(2);
    let (raw_tx, raw_rx) = bounded::<RawItem>(read_bound);
    let (enc_tx, enc_rx) = bounded::<EncodedEntry>(cpu_bound);
    let (feed_tx, feed_rx) = bounded::<(u64, WorkItem)>(read_bound);

    // 4. Feeder: batch mode replays a sorted Vec; streaming mode wraps the
    // jwalk walker on a background thread and pipes discoveries in real time.
    let index_counter = Arc::new(AtomicU64::new(0));
    let discovered_bytes = Arc::new(AtomicU64::new(0));
    let discovered_files = Arc::new(AtomicU64::new(0));

    let feeder = {
        let feed_tx = feed_tx.clone();
        let index_counter = Arc::clone(&index_counter);
        let discovered_bytes = Arc::clone(&discovered_bytes);
        let discovered_files = Arc::clone(&discovered_files);
        if let Some(items) = batch_items {
            thread::spawn(move || -> Result<()> {
                for it in items {
                    let idx = index_counter.fetch_add(1, AtomicOrdering::Relaxed);
                    discovered_bytes.fetch_add(it.size, AtomicOrdering::Relaxed);
                    discovered_files.fetch_add(1, AtomicOrdering::Relaxed);
                    if feed_tx.send((idx, it)).is_err() {
                        break;
                    }
                }
                Ok(())
            })
        } else {
            // Streaming walker → wrap send with index assignment
            let (wtx, wrx) = bounded::<WorkItem>(read_bound.max(16));
            let roots = opts.paths.clone();
            let exclude = opts.exclude.clone();
            let walk_jobs = opts.walk_jobs;
            let use_bulk = cfg!(target_os = "macos") && !opts.classic_walk;
            let walker_thread = thread::spawn(move || -> Result<()> {
                #[cfg(target_os = "macos")]
                {
                    if use_bulk {
                        return crate::bulk_walker::walk_stream_bulk(&roots, &exclude, wtx);
                    }
                }
                let _ = use_bulk;
                walker::walk_stream(&roots, &exclude, walk_jobs, wtx)
            });
            let feed_tx2 = feed_tx.clone();
            let idx_c = Arc::clone(&index_counter);
            let db = Arc::clone(&discovered_bytes);
            let df = Arc::clone(&discovered_files);
            let bridge = thread::spawn(move || -> Result<()> {
                while let Ok(it) = wrx.recv() {
                    let idx = idx_c.fetch_add(1, AtomicOrdering::Relaxed);
                    db.fetch_add(it.size, AtomicOrdering::Relaxed);
                    df.fetch_add(1, AtomicOrdering::Relaxed);
                    if feed_tx2.send((idx, it)).is_err() {
                        break;
                    }
                }
                walker_thread
                    .join()
                    .map_err(|_| anyhow::anyhow!("walker thread panicked"))??;
                Ok(())
            });
            bridge
        }
    };
    drop(feed_tx);

    // 5. Reader pool
    #[cfg(target_os = "macos")]
    let source: Arc<dyn Source> = if opts.dispatch_io {
        Arc::new(crate::dispatch_io::DispatchIoSource { keep_cache: opts.keep_cache })
    } else {
        Arc::new(LocalFsSource { keep_cache: opts.keep_cache })
    };
    #[cfg(not(target_os = "macos"))]
    let source: Arc<dyn Source> = Arc::new(LocalFsSource { keep_cache: opts.keep_cache });
    let mut reader_handles = Vec::new();
    for _ in 0..opts.read_jobs {
        let feed_rx = feed_rx.clone();
        let raw_tx = raw_tx.clone();
        let source = Arc::clone(&source);
        reader_handles.push(thread::spawn(move || -> Result<()> {
            while let Ok((index, item)) = feed_rx.recv() {
                let bytes = source.read(&item)?;
                if raw_tx.send(RawItem { index, item, bytes }).is_err() {
                    break;
                }
            }
            Ok(())
        }));
    }
    drop(feed_rx);
    drop(raw_tx);

    // 6. CPU pool: compress + optional AES-256 encryption.
    //
    // Each worker owns a `Scratch` so buffers (deflate output, deflate
    // compressor state) are reused across items — near-zero allocator
    // pressure on many-file corpora.
    let password = opts.password.clone();
    let method = opts.method;
    let level = opts.level;
    // Auto parallel-block DEFLATE when a single large file dominates the
    // workload. In streaming mode we don't know the count up front, so we
    // gate on the batched total. If unset (streaming), the CPU worker will
    // still fall back to single-shot compress — parallel-block is opt-in.
    let single_file_parallel = matches!(method, Method::Deflate)
        && total_files_hint == 1
        && total_bytes_hint >= compress::PARALLEL_DEFLATE_THRESHOLD;
    let mut cpu_handles = Vec::new();
    for worker_id in 0..opts.cpu_jobs {
        let raw_rx = raw_rx.clone();
        let enc_tx = enc_tx.clone();
        let password = password.clone();
        let tui_handle = tui_handle.clone();
        cpu_handles.push(thread::spawn(move || -> Result<()> {
            let mut scratch = Scratch::new();
            while let Ok(raw) = raw_rx.recv() {
                let raw_len = raw.bytes.len();
                if let Some(t) = &tui_handle {
                    t.worker_started(worker_id, raw.item.name_in_archive.clone(), raw_len as u64);
                }
                let started = std::time::Instant::now();
                let entry = encode_one(
                    &raw,
                    method,
                    level,
                    password.as_deref(),
                    &mut scratch,
                    single_file_parallel,
                )?;
                if let Some(t) = &tui_handle {
                    let elapsed = started.elapsed().as_secs_f64();
                    let mb_per_s = if elapsed > 0.0 {
                        (raw_len as f64 / 1_048_576.0) / elapsed
                    } else {
                        0.0
                    };
                    t.worker_finished(worker_id, mb_per_s);
                }
                if enc_tx.send(entry).is_err() {
                    break;
                }
            }
            Ok(())
        }));
    }
    drop(raw_rx);
    drop(enc_tx);

    // 7. Writer loop (on the main thread) — single writer, serial writes.
    //
    // With `--sort` we re-order arrivals into walk-index order so archives
    // are byte-identical across runs. Without `--sort` we write in arrival
    // order for minimum latency.
    let mut wrote_files = 0u64;
    if opts.sort {
        use std::collections::HashMap;
        let mut pending: HashMap<u64, EncodedEntry> = HashMap::new();
        let mut next_idx: u64 = 0;
        while let Ok(entry) = enc_rx.recv() {
            pending.insert(entry.index, entry);
            while let Some(e) = pending.remove(&next_idx) {
                wrote_files += 1;
                let name = e.name.clone();
                let plain_size = e.uncompressed_size;
                zw.write_entry(&e)?;
                let total = discovered_files.load(AtomicOrdering::Relaxed).max(wrote_files);
                progress.inc(plain_size);
                progress.set_msg(format!("{}/{} files — {}", wrote_files, total, name));
                if opts.verbose {
                    eprintln!("added {}", name);
                }
                next_idx += 1;
            }
        }
    } else {
        while let Ok(entry) = enc_rx.recv() {
            wrote_files += 1;
            let name = entry.name.clone();
            let plain_size = entry.uncompressed_size;
            zw.write_entry(&entry)?;
            let total = discovered_files.load(AtomicOrdering::Relaxed).max(wrote_files);
            progress.inc(plain_size);
            progress.set_msg(format!("{}/{} files — {}", wrote_files, total, name));
            if opts.verbose {
                eprintln!("added {}", name);
            }
        }
    }

    // 8. Join all
    feeder
        .join()
        .map_err(|_| anyhow::anyhow!("feeder thread panicked"))??;
    for h in reader_handles {
        h.join().ok().transpose()?;
    }
    for h in cpu_handles {
        h.join().ok().transpose()?;
    }

    let inner = zw.finish()?;
    let mut buf_writer = inner;
    use std::io::{Seek, SeekFrom, Write};
    buf_writer.flush()?;
    // Trim any unused preallocated tail so the archive's on-disk size
    // matches its logical size. Ignore errors — worst case the file uses
    // slightly more disk than st_size reports until next mount cycle.
    let actual_len = buf_writer.stream_position().unwrap_or(0);
    if actual_len > 0 {
        let file = buf_writer.get_mut();
        let _ = file.set_len(actual_len);
        let _ = file.seek(SeekFrom::End(0));
    }

    progress.finish("done");
    if let Some(t) = tui_handle {
        t.finish();
    }
    Ok(())
}

/// Pick BufWriter capacity for the output based on filesystem info.
fn choose_writer_capacity(info: Option<&platform::FsInfo>) -> usize {
    const MIN: usize = 4 * 1024 * 1024;
    const EXFAT_CAP: usize = 16 * 1024 * 1024;
    let Some(info) = info else {
        return MIN;
    };
    let fs = info.fs_type.to_ascii_lowercase();
    if fs == "exfat" || fs == "msdos" {
        return EXFAT_CAP;
    }
    let scaled = (info.iosize as usize).saturating_mul(4);
    scaled.max(MIN)
}

/// Auto-adjust defaults based on the source/output filesystem when the user
/// hasn't set the relevant flags explicitly. Prints a one-line note when
/// anything changes so behavior stays transparent.
///
/// Current rules (macOS only — non-macOS defaults are already reasonable):
///   * source or output on APFS-over-USB → keep_cache=true, read_jobs=1,
///     dispatch_io=true. F_NOCACHE and multi-reader thrash on this combo.
///   * source or output on NTFS-on-macOS → keep_cache=true. Apple's stock
///     NTFS driver is heavily page-cache-oriented; F_NOCACHE hurts reads.
fn auto_tune(mut opts: Options) -> Options {
    #[cfg(not(target_os = "macos"))]
    {
        return opts;
    }

    #[cfg(target_os = "macos")]
    {
        use crate::usb_info::probe_path;

        // Collect fs snapshots for every input + the output's parent dir.
        let mut probes: Vec<crate::usb_info::SourceReport> = Vec::new();
        for p in &opts.paths {
            if let Ok(r) = probe_path(p) {
                probes.push(r);
            }
        }
        let out_parent = opts
            .archive
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        if let Ok(r) = probe_path(&out_parent) {
            probes.push(r);
        }

        let apfs_on_usb = probes.iter().any(|r| {
            r.fs_type.eq_ignore_ascii_case("apfs")
                && r.bus_protocol.as_deref() == Some("USB")
        });
        let ntfs_on_mac = probes
            .iter()
            .any(|r| r.fs_type.to_ascii_lowercase().contains("ntfs"));

        let mut changes: Vec<String> = Vec::new();

        if apfs_on_usb {
            if !opts.user_flags.keep_cache && !opts.keep_cache {
                opts.keep_cache = true;
                changes.push("keep_cache=on".into());
            }
            if !opts.user_flags.dispatch_io && !opts.dispatch_io {
                opts.dispatch_io = true;
                changes.push("dispatch_io=on".into());
            }
            // With dispatch_io enabled, each reader submits a synchronous
            // dispatch_io_read and blocks on its completion semaphore. One
            // reader = one in-flight read at a time; GCD can't pipeline.
            // Bump reader count so the queue stays populated. Without
            // dispatch_io, keep the conservative anti-thrash value.
            if !opts.user_flags.read_jobs {
                let target = if opts.dispatch_io { 4 } else { 1 };
                if opts.read_jobs != target {
                    opts.read_jobs = target;
                    changes.push(format!("read_jobs={}", target));
                }
            }
        } else if ntfs_on_mac {
            if !opts.user_flags.keep_cache && !opts.keep_cache {
                opts.keep_cache = true;
                changes.push("keep_cache=on".into());
            }
        }

        if !changes.is_empty() && !opts.quiet {
            let reason = if apfs_on_usb { "APFS-on-USB" } else { "NTFS-on-macOS" };
            eprintln!(
                "tzip: auto-tuned defaults for {} source ({}). Override with the same flag.",
                reason,
                changes.join(", ")
            );
        }
        opts
    }
}

fn encode_one(
    raw: &RawItem,
    method: Method,
    level: u32,
    password: Option<&str>,
    scratch: &mut Scratch,
    single_file_parallel: bool,
) -> Result<EncodedEntry> {
    let uncompressed_size = raw.bytes.len() as u64;

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&raw.bytes);
    let crc32 = hasher.finalize();

    let out = if single_file_parallel {
        compress::compress_parallel_deflate(&raw.bytes, level)?
    } else {
        compress::compress(method, level, &raw.bytes, scratch)?
    };
    let inner_method = compress::method_id(method);
    let version_needed = compress::version_needed(method);

    let (body, encrypted, method_on_wire) = match password {
        Some(pw) if !pw.is_empty() => {
            // Single-alloc AES: extend the compressed buffer in-place with
            // room for salt+verify prefix and the mac suffix.
            let compressed_len = out.data.len();
            let mut buf = out.data;
            // Shift compressed data right by SALT+VERIFY, reserve MAC at end.
            buf.reserve(crypto::AE2_OVERHEAD);
            let prefix = crypto::SALT_LEN + crypto::VERIFY_LEN;
            buf.resize(compressed_len + crypto::AE2_OVERHEAD, 0);
            buf.copy_within(0..compressed_len, prefix);
            crypto::encrypt_ae2_in_place(pw, &mut buf, compressed_len)?;
            (buf, true, METHOD_AES)
        }
        _ => (out.data, false, inner_method),
    };

    Ok(EncodedEntry {
        index: raw.index,
        name: raw.item.name_in_archive.clone(),
        body,
        uncompressed_size,
        crc32,
        method_on_wire,
        inner_method,
        encrypted,
        gp_bit1: out.gp_bit1,
        version_needed,
        mtime: raw.item.mtime,
    })
}

