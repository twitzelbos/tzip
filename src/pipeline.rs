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

use crate::method::Method;
use crate::options::Options;
use crate::compress::{self, Scratch};
use crate::crypto;
use crate::platform;
use crate::progress::Progress;
use crate::sevenz;
use crate::tui;
use crate::walker::{self, WalkOpts, WorkItem};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use crate::zipformat::METHOD_AES;
use crate::zipwriter::{EncodedEntry, ZipWriter};

pub use crate::source::{LocalFsSource, RawItem, Source};

/// Choose BufWriter capacity for the output based on filesystem info.
/// Shared with the update module.
pub(crate) fn writer_capacity_for(info: Option<&platform::FsInfo>) -> usize {
    choose_writer_capacity(info)
}

/// Create and update stay local-only. Reject URL-shaped archive paths
/// upfront with a clear error pointing users at the right subcommand.
pub(crate) fn reject_remote_archive(archive: &std::path::Path) -> Result<()> {
    let s = archive.to_string_lossy();
    for scheme in &["http://", "https://", "s3://"] {
        if s.starts_with(scheme) {
            anyhow::bail!(
                "remote writes are not supported — {} is a URL; \
                 only `ls`, `cat`, and `x` work on remote URLs",
                s
            );
        }
    }
    Ok(())
}

/// Shared single-file encoder used by both the main pipeline and update mode.
pub(crate) fn encode_item(
    raw: RawItem,
    method: Method,
    level: u32,
    password: Option<&str>,
    scratch: &mut Scratch,
    single_file_parallel: bool,
    want_md5: bool,
    want_sha256: bool,
    index_terms_match: bool,
    want_facet_body: bool,
) -> Result<EncodedEntry> {
    encode_one(
        raw,
        method,
        level,
        password,
        scratch,
        single_file_parallel,
        want_md5,
        want_sha256,
        index_terms_match,
        want_facet_body,
    )
}

/// One-shot tracing: when TZIP_PROFILE=1, print each stage's wall
/// time to stderr. Zero-cost when the env var is absent.
fn profile(enabled: bool, stage: &str, start: std::time::Instant) {
    if enabled {
        eprintln!(
            "tzip profile: {:20} {:>8.3} ms",
            stage,
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
}

pub fn run(opts: Options) -> Result<()> {
    let profile_on = std::env::var("TZIP_PROFILE").ok().as_deref() == Some("1");
    let overall_start = std::time::Instant::now();
    reject_remote_archive(&opts.archive)?;

    // `tzip u` dispatch happens in main.rs now: if the user ran the
    // `u` subcommand and the archive exists, main calls
    // `update::run_update` directly; the create-mode pipeline only
    // runs here when the archive doesn't exist yet (`u` on a
    // non-existent archive falls through to create, matching
    // `7z u`). Removing the branch here broke the historical
    // `pipeline → update` dependency edge that otherwise wedged
    // the whole lib into one module cycle.

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
    let t_autotune = std::time::Instant::now();
    let opts = auto_tune(opts);
    profile(profile_on, "auto_tune", t_autotune);

    // .7z solid needs the full list up front (it's serial LZMA2 and the
    // sevenz-rust API takes an ordered iterator). Use the batch walk.
    if opts.solid {
        let items = walker::walk(WalkOpts {
            roots: &opts.paths,
            exclude: &opts.exclude,
            sort: opts.sort,
            walk_threads: opts.walk_jobs.max(1),
            basename_only: opts.basename_only,
        })?;
        if items.is_empty() {
            anyhow::bail!("no files to archive");
        }
        let total_bytes: u64 = items.iter().map(|i| i.size).sum();
        let total_files = items.len() as u64;
        return sevenz::write_solid(&opts, &items, total_bytes, total_files);
    }

    // Try to open a `--raw-block` source up front (macOS + raw-apfs
    // feature). When available, we also do the walk via the raw parser
    // and switch the pipeline into batch mode with those items — this
    // bypasses `bulk_walker`'s VFS syscalls entirely, so the walk stage
    // stops competing with the read stage for macOS Endpoint Security
    // auth hooks. Failure falls back to the normal reader + walker.
    #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
    let raw_apfs_source: Option<Arc<crate::raw_apfs::RawApfsSource>> = {
        // Pool = reader-thread count so `pool_recv.recv()` is contention-free.
        // Cap at 32 to keep ~50 ms/volume open costs bounded.
        let pool_size = opts.read_jobs.max(4).min(32);

        // Device mode (both OSes): --apfs-device points straight at the raw
        // block/char device — no mount needed. This is the only way to read an
        // *unmounted* APFS volume (the common Linux case). A failure here is
        // fatal: there's no VFS fallback for an unmounted device.
        if let Some(dev) = opts.apfs_device.clone() {
            match crate::raw_apfs::RawApfsSource::open_for_device(&dev, pool_size, opts.verbose) {
                Ok(s) => {
                    if opts.verbose {
                        eprintln!(
                            "tzip: --apfs-device active for {} (pool size {})",
                            dev.display(),
                            pool_size
                        );
                    }
                    Some(Arc::new(s))
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "open APFS device {} (raw device is root-only; run with sudo \
                         or grant read access)",
                        dev.display()
                    )));
                }
            }
        } else {
            // macOS mount mode: resolve the raw device backing a mounted APFS
            // volume from the first source path. Failure falls back to the
            // normal VFS reader.
            #[cfg(target_os = "macos")]
            {
                if opts.raw_block {
                    let first = opts
                        .paths
                        .first()
                        .cloned()
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    match crate::platform::fs_info(&first) {
                        Ok(info) => match crate::raw_apfs::RawApfsSource::open_for_mount(
                            &info.mount_point,
                            pool_size,
                            opts.verbose,
                        ) {
                            Ok(s) => {
                                if opts.verbose {
                                    eprintln!(
                                        "tzip: --raw-block active for {} (pool size {})",
                                        info.mount_point.display(),
                                        pool_size
                                    );
                                }
                                Some(Arc::new(s))
                            }
                            Err(e) => {
                                eprintln!("tzip: --raw-block unavailable ({e:#}) — falling back");
                                None
                            }
                        },
                        Err(e) => {
                            eprintln!("tzip: --raw-block fs_info failed ({e:#}) — falling back");
                            None
                        }
                    }
                } else {
                    None
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        }
    };

    // Batch-mode item list. Set only for `--sort` (which needs the full
    // list up front to reorder). Streaming is the default, and the
    // streaming feeder below will use `RawApfsSource::walk_stream` when
    // `--raw-block` is on and skip `bulk_walker` entirely.
    #[allow(unused_mut)]
    let batch_items: Option<Vec<WorkItem>> = if opts.sort {
        // Raw-APFS mode (esp. an unmounted `--apfs-device`): the VFS walker
        // can't `stat` in-volume paths, so gather items from the parser's own
        // walker and sort them here by archive path (matching `walker::walk`).
        let mut collected: Option<Vec<WorkItem>> = None;
        #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
        {
            if let Some(src) = raw_apfs_source.clone() {
                let (wtx, wrx) = crossbeam_channel::unbounded::<WorkItem>();
                let roots = opts.paths.clone();
                let exclude = opts.exclude.clone();
                let basename_only = opts.basename_only;
                let h = thread::spawn(move || src.walk_stream(&roots, &exclude, basename_only, wtx));
                let mut items: Vec<WorkItem> = wrx.iter().collect();
                h.join()
                    .map_err(|_| anyhow::anyhow!("apfs walk thread panicked"))??;
                items.sort_by(|a, b| a.name_in_archive.cmp(&b.name_in_archive));
                collected = Some(items);
            }
        }
        let items = match collected {
            Some(v) => v,
            None => walker::walk(WalkOpts {
                roots: &opts.paths,
                exclude: &opts.exclude,
                sort: opts.sort,
                walk_threads: opts.walk_jobs.max(1),
                basename_only: opts.basename_only,
            })?,
        };
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
        opts.quiet || opts.tui,
    ));
    let tui_handle = if opts.tui {
        Some(tui::start(total_bytes_hint, total_files_hint, opts.cpu_jobs))
    } else {
        None
    };

    // 2. Open archive + best-effort preallocation + FS-aware buffer sizing
    let t_open = std::time::Instant::now();
    let file = File::create(&opts.archive)
        .with_context(|| format!("create archive {}", opts.archive.display()))?;
    profile(profile_on, "File::create", t_open);
    let t_fsinfo = std::time::Instant::now();
    // `fstatfs` on the fd we just opened — skips the kernel's second
    // path walk that `fs_info(parent_dir)` would trigger. Same result
    // (file + its parent dir are on the same filesystem).
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
    profile(profile_on, "fs_info", t_fsinfo);

    // Preallocate extents if we know an upper-bound size. Upper-bound for
    // any compression method is `uncompressed_total × 1.05` (AES adds
    // ~28 bytes/entry + CDR overhead). Streaming mode with unknown total
    // preallocates a modest 256 MiB default.
    let preallocate_len: u64 = if total_bytes_hint > 0 {
        (total_bytes_hint as f64 * 1.05).round() as u64
    } else {
        256 * 1024 * 1024
    };
    let t_prealloc = std::time::Instant::now();
    let _ = platform::preallocate(&file, preallocate_len);
    profile(profile_on, "preallocate", t_prealloc);

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
            // Streaming walker → wrap send with index assignment.
            // When `--raw-block` is on and the raw source opened, we
            // walk via the raw APFS parser (no VFS syscalls → macOS
            // Endpoint Security has nothing to hook). Otherwise fall
            // back to `bulk_walker` (getattrlistbulk) or classic jwalk.
            let (wtx, wrx) = bounded::<WorkItem>(read_bound.max(64));
            let roots = opts.paths.clone();
            let exclude = opts.exclude.clone();
            let walk_jobs = opts.walk_jobs;
            let use_bulk = cfg!(target_os = "macos") && !opts.classic_walk;
            let basename_only = opts.basename_only;
            #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
            let raw_src_for_walk = raw_apfs_source.clone();
            let walker_thread = thread::spawn(move || -> Result<()> {
                #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
                {
                    if let Some(src) = raw_src_for_walk {
                        return src.walk_stream(&roots, &exclude, basename_only, wtx);
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    if use_bulk {
                        return crate::bulk_walker::walk_stream_bulk(&roots, &exclude, basename_only, wtx);
                    }
                }
                let _ = use_bulk;
                walker::walk_stream(&roots, &exclude, walk_jobs, basename_only, wtx)
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

    // 5. Reader pool.
    //
    // Source precedence:
    //   * raw-APFS source — macOS mount `--raw-block`, or `--apfs-device` on
    //     either OS (reads by parsing APFS off the raw device);
    //   * Linux FIEMAP extent-order / io_uring source (`--raw-block` /
    //     `--io-uring`);
    //   * otherwise the default VFS reader (dispatch_io on macOS if asked).

    // Linux extent-order / io_uring source. Not built when a raw-APFS device
    // source is already taking over the reads.
    #[cfg(target_os = "linux")]
    let want_linux_bulk = {
        let apfs_active = {
            #[cfg(feature = "raw-apfs")]
            {
                raw_apfs_source.is_some()
            }
            #[cfg(not(feature = "raw-apfs"))]
            {
                false
            }
        };
        if apfs_active {
            false
        } else {
            #[cfg(feature = "io-uring")]
            {
                opts.raw_block || opts.io_uring
            }
            #[cfg(not(feature = "io-uring"))]
            {
                opts.raw_block
            }
        }
    };
    // Probe io_uring once: if the kernel/environment doesn't support it, fall
    // back to blocking reads (still extent-ordered) rather than failing the run.
    #[cfg(all(target_os = "linux", feature = "io-uring"))]
    let use_io_uring = if opts.io_uring && !crate::io_uring_src::available() {
        if !opts.quiet {
            eprintln!(
                "tzip: io_uring unavailable on this kernel — falling back to blocking reads"
            );
        }
        false
    } else {
        opts.io_uring
    };
    #[cfg(target_os = "linux")]
    let linux_raw_source: Option<Arc<crate::linux_raw::LinuxExtentSource>> = if want_linux_bulk {
        Some(Arc::new(crate::linux_raw::LinuxExtentSource {
            keep_cache: opts.keep_cache,
            verbose: opts.verbose,
            #[cfg(feature = "io-uring")]
            io_uring: use_io_uring,
        }))
    } else {
        None
    };

    // --- source selection ---
    #[cfg(target_os = "macos")]
    let source: Arc<dyn Source> = {
        #[cfg(feature = "raw-apfs")]
        {
            if let Some(src) = raw_apfs_source.clone() {
                let s: Arc<dyn Source> = src;
                s
            } else if opts.dispatch_io {
                Arc::new(crate::dispatch_io::DispatchIoSource { keep_cache: opts.keep_cache })
            } else {
                Arc::new(LocalFsSource { keep_cache: opts.keep_cache })
            }
        }
        #[cfg(not(feature = "raw-apfs"))]
        {
            if opts.dispatch_io {
                Arc::new(crate::dispatch_io::DispatchIoSource { keep_cache: opts.keep_cache })
            } else {
                Arc::new(LocalFsSource { keep_cache: opts.keep_cache })
            }
        }
    };
    #[cfg(target_os = "linux")]
    let source: Arc<dyn Source> = {
        #[cfg(feature = "raw-apfs")]
        {
            if let Some(src) = raw_apfs_source.clone() {
                let s: Arc<dyn Source> = src;
                s
            } else if let Some(s) = &linux_raw_source {
                let s: Arc<dyn Source> = s.clone();
                s
            } else {
                Arc::new(LocalFsSource { keep_cache: opts.keep_cache })
            }
        }
        #[cfg(not(feature = "raw-apfs"))]
        {
            if let Some(s) = &linux_raw_source {
                let s: Arc<dyn Source> = s.clone();
                s
            } else {
                Arc::new(LocalFsSource { keep_cache: opts.keep_cache })
            }
        }
    };
    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    let source: Arc<dyn Source> = Arc::new(LocalFsSource { keep_cache: opts.keep_cache });

    // --- bulk vs per-item ---
    // Bulk mode drains the feed, splits it into N contiguous walker-order
    // chunks, and lets each reader thread reorder its chunk into physical-disk
    // order via the source's `read_bulk`. Engaged whenever a reordering-capable
    // source is active (raw-APFS or the Linux extent/io_uring reader).
    #[allow(unused_mut)]
    let use_bulk = {
        let mut b = false;
        #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
        {
            b = b || raw_apfs_source.is_some();
        }
        #[cfg(target_os = "linux")]
        {
            b = b || linux_raw_source.is_some();
        }
        b
    };

    let mut reader_handles = Vec::new();
    if use_bulk {
        // One splitter thread drains the feed into N contiguous chunks; N
        // reader threads each reorder + read their chunk. --read-jobs 1 gives
        // a single forward sweep (spinning / USB); higher overlaps sweeps.
        let feed_rx = feed_rx.clone();
        let n = opts.read_jobs.max(1);
        let (chunk_tx, chunk_rx) = bounded::<Vec<(u64, WorkItem)>>(n.max(1));
        reader_handles.push(thread::spawn(move || -> Result<()> {
            let mut items: Vec<(u64, WorkItem)> = Vec::new();
            while let Ok((index, item)) = feed_rx.recv() {
                items.push((index, item));
            }
            // Move chunks out of the Vec without cloning. The prior
            // `items[start..end].to_vec()` cloned every WorkItem
            // (`PathBuf` + `String` + optional `Arc`) per chunk; on a
            // 100k-entry corpus that's a lot of allocator traffic
            // before any work actually starts. `split_off` just does
            // a bulk memmove on the pointer-sized `(u64, WorkItem)`
            // tuples — WorkItem contents are moved, not cloned.
            let total = items.len();
            if total > 0 {
                let per = (total + n - 1) / n;
                let mut remaining = items;
                for _ in 0..n {
                    if remaining.is_empty() {
                        break;
                    }
                    let take = per.min(remaining.len());
                    // split_off(at) returns `[at..]`, keeps `[..at]`
                    // in self. Swap so `chunk` becomes the head and
                    // `remaining` continues with the tail.
                    let rest = remaining.split_off(take);
                    let chunk = std::mem::replace(&mut remaining, rest);
                    if chunk_tx.send(chunk).is_err() {
                        break;
                    }
                }
            }
            drop(chunk_tx);
            Ok(())
        }));
        for _ in 0..n {
            let chunk_rx = chunk_rx.clone();
            let raw_tx = raw_tx.clone();
            let source = Arc::clone(&source);
            reader_handles.push(thread::spawn(move || -> Result<()> {
                while let Ok(chunk) = chunk_rx.recv() {
                    source.read_bulk(chunk, raw_tx.clone())?;
                }
                Ok(())
            }));
        }
    } else {
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
    let want_md5 = opts.write_md5;
    let want_sha256 = opts.write_sha256;
    // Share `--index-terms` globs across worker threads. Compiled once
    // here; each worker checks per-entry whether the name matches any
    // of them and tokenises the plaintext when it does.
    let index_terms_globs: std::sync::Arc<Vec<String>> =
        std::sync::Arc::new(opts.index_terms.clone());
    let want_facet = !opts.facet.is_empty();
    // Phase 5 columnar sidecar: parse the schema up front so a bad
    // schema fails the create before any bodies get encoded. The
    // builder reuses the same plaintext-body feed as facets — a body
    // that's shipped for facet aggregation is also JSON that the
    // columnar extractor can walk.
    //
    // Phase 5c: `--write-columnar` with an empty path is the
    // auto-infer sentinel (set by cli when the user passes the bool
    // flag with no `--columnar-schema SCHEMA.json` override). In
    // that case we sample the first N JSON entries matching
    // `--index-terms` and derive a `ColumnarSchema` from them.
    let (want_columnar, columnar_builder) = match &opts.write_columnar {
        Some(schema_path) if !schema_path.as_os_str().is_empty() => {
            let bytes = std::fs::read(schema_path).with_context(|| {
                format!("read columnar schema {}", schema_path.display())
            })?;
            let schema = crate::columnar::ColumnarSchema::parse(&bytes)?;
            (true, Some(crate::columnar::ColumnarBuilder::new(schema)))
        }
        Some(_) => {
            // Auto-infer. Needs `--index-terms` to know which entries
            // are JSON-shaped — without it we'd be guessing.
            if opts.index_terms.is_empty() {
                anyhow::bail!(
                    "columnar auto-infer requires at least one `--index-terms GLOB` so tzip knows which entries are JSON. \
                     Pass `--index-terms '*.json'` (and/or any other globs), or hand-author a schema with `--columnar-schema SCHEMA.json`."
                );
            }
            let schema = infer_columnar_schema(&opts)?;
            if !opts.quiet {
                let cols: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
                eprintln!(
                    "tzip create: inferred columnar schema with {} column(s): {}",
                    cols.len(),
                    cols.join(", "),
                );
            }
            (true, Some(crate::columnar::ColumnarBuilder::new(schema)))
        }
        None => (false, None),
    };
    let mut columnar_builder = columnar_builder;
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
        let index_terms_globs = std::sync::Arc::clone(&index_terms_globs);
        cpu_handles.push(thread::spawn(move || -> Result<()> {
            let mut scratch = Scratch::new();
            while let Ok(raw) = raw_rx.recv() {
                let raw_len = raw.bytes.len();
                if let Some(t) = &tui_handle {
                    t.worker_started(worker_id, raw.item.name_in_archive.clone(), raw_len as u64);
                }
                let started = std::time::Instant::now();
                let index_terms_match = !index_terms_globs.is_empty()
                    && index_terms_globs
                        .iter()
                        .any(|g| crate::walker::is_excluded(&raw.item.name_in_archive, std::slice::from_ref(g)));
                // Only ship plaintext up to the main thread when
                // facet or columnar extraction is on AND the name
                // matched the term-index glob (bodies on this glob
                // are assumed JSON for structured-field purposes).
                let want_facet_body = (want_facet || want_columnar) && index_terms_match;
                let entry = encode_one(
                    raw,
                    method,
                    level,
                    password.as_deref(),
                    &mut scratch,
                    single_file_parallel,
                    want_md5,
                    want_sha256,
                    index_terms_match,
                    want_facet_body,
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
    profile(profile_on, "setup_until_loop", overall_start);
    let t_mainloop = std::time::Instant::now();
    let mut wrote_files = 0u64;
    // Track names as entries land so we can build a `.tzip/names.v1`
    // index entry once encoding finishes, when `--index-names` is on.
    // Allocated even when the feature isn't requested — the Vec is
    // empty in that case and the end-of-loop block is skipped.
    let mut indexed_names: Vec<String> = Vec::new();
    // Analogue for the Phase 2 `--index-terms` term index. Workers
    // tokenised matching entries and attached the tokens to each
    // `EncodedEntry.index_terms`; here we drain them into the
    // builder with the final CDR-order entry index as the posting
    // identifier.
    let mut term_builder = crate::termidx::TermIndexBuilder::new();
    // Phase 3 facet builders. One per `--facet` path. Workers ship
    // plaintext up via `EncodedEntry.facet_body`; this loop feeds
    // every requested facet from every matching entry.
    let mut facet_builders: Vec<crate::facets::FacetBuilder> = opts
        .facet
        .iter()
        .map(|spec| crate::facets::FacetBuilder::new(spec))
        .collect::<Result<Vec<_>>>()?;
    if opts.sort {
        use std::collections::HashMap;
        let mut pending: HashMap<u64, EncodedEntry> = HashMap::new();
        let mut next_idx: u64 = 0;
        while let Ok(entry) = enc_rx.recv() {
            pending.insert(entry.index, entry);
            while let Some(mut e) = pending.remove(&next_idx) {
                wrote_files += 1;
                let plain_size = e.uncompressed_size;
                let total = discovered_files.load(AtomicOrdering::Relaxed).max(wrote_files);
                let msg = format!("{}/{} files — {}", wrote_files, total, e.name);
                if opts.verbose {
                    eprintln!("added {}", e.name);
                }
                if opts.index_names {
                    indexed_names.push(e.name.clone());
                }
                if let Some(tokens) = e.index_terms.take() {
                    let cdr_pos = (wrote_files - 1) as u32;
                    term_builder.add_prepared(cdr_pos, tokens);
                }
                if let Some(body) = e.facet_body.take() {
                    for fb in &mut facet_builders {
                        fb.add_body(&body);
                    }
                    if let Some(cb) = columnar_builder.as_mut() {
                        cb.add_body(&body);
                    }
                }
                zw.write_entry(e)?;
                progress.inc(plain_size);
                progress.set_msg(msg);
                next_idx += 1;
            }
        }
    } else {
        while let Ok(mut entry) = enc_rx.recv() {
            wrote_files += 1;
            let plain_size = entry.uncompressed_size;
            let total = discovered_files.load(AtomicOrdering::Relaxed).max(wrote_files);
            let msg = format!("{}/{} files — {}", wrote_files, total, entry.name);
            if opts.verbose {
                eprintln!("added {}", entry.name);
            }
            if opts.index_names {
                indexed_names.push(entry.name.clone());
            }
            if let Some(tokens) = entry.index_terms.take() {
                let cdr_pos = (wrote_files - 1) as u32;
                term_builder.add_prepared(cdr_pos, tokens);
            }
            if let Some(body) = entry.facet_body.take() {
                for fb in &mut facet_builders {
                    fb.add_body(&body);
                }
                if let Some(cb) = columnar_builder.as_mut() {
                    cb.add_body(&body);
                }
            }
            zw.write_entry(entry)?;
            progress.inc(plain_size);
            progress.set_msg(msg);
        }
    }

    // --facet: emit one `.tzip/facets.v1/<leaf>` entry per facet.
    // These are STORE-method entries (never encrypted or compressed)
    // so `Archive::try_load_facet_indices` can read them directly.
    for fb in facet_builders.drain(..) {
        let name = crate::facets::entry_name_for(fb.field_name());
        let payload = fb.finish()?;
        if payload.is_empty() {
            continue;
        }
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&payload);
        let crc = hasher.finalize();
        let body_len = payload.len() as u64;
        let e = crate::zipwriter::EncodedEntry {
            index: u64::MAX,
            name: name.clone(),
            body: payload,
            uncompressed_size: body_len,
            crc32: crc,
            method_on_wire: 0,
            inner_method: 0,
            encrypted: false,
            gp_bit1: false,
            version_needed: 20,
            mtime: (0, 0),
            external_attr: 0o100644u32 << 16,
            md5_digest: None,
            sha256_digest: None,
            index_terms: None,
            facet_body: None,
        };
        if opts.index_names {
            indexed_names.push(name);
        }
        zw.write_entry(e)?;
    }

    // --write-columnar: emit `.tzip/columnar.v1.parquet` (Phase 5).
    // Built from the same JSON bodies that fed the facets; shared
    // plaintext snapshot keeps the extra extraction cost ~zero when
    // facets are already on. Emitted before the term index so the
    // name-index MPH covers all sentinels.
    if let Some(cb) = columnar_builder.take() {
        if cb.row_count() > 0 {
            let payload = cb.finish()?;
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&payload);
            let crc = hasher.finalize();
            let body_len = payload.len() as u64;
            let e = crate::zipwriter::EncodedEntry {
                index: u64::MAX,
                name: crate::columnar::INDEX_ENTRY_NAME.to_string(),
                body: payload,
                uncompressed_size: body_len,
                crc32: crc,
                method_on_wire: 0,
                inner_method: 0,
                encrypted: false,
                gp_bit1: false,
                version_needed: 20,
                mtime: (0, 0),
                external_attr: 0o100644u32 << 16,
                md5_digest: None,
                sha256_digest: None,
                index_terms: None,
                facet_body: None,
            };
            if opts.index_names {
                indexed_names.push(e.name.clone());
            }
            zw.write_entry(e)?;
        }
    }

    // --index-terms: emit the `.tzip/terms.v1` entry if we collected
    // any tokens. Must happen BEFORE `.tzip/names.v1` so the latter's
    // MPH covers `.tzip/terms.v1` too (if name-indexing is on).
    if !opts.index_terms.is_empty() {
        let payload = term_builder.finish()?;
        if !payload.is_empty() {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&payload);
            let crc = hasher.finalize();
            let body_len = payload.len() as u64;
            let e = crate::zipwriter::EncodedEntry {
                index: u64::MAX,
                name: crate::termidx::INDEX_ENTRY_NAME.to_string(),
                body: payload,
                uncompressed_size: body_len,
                crc32: crc,
                method_on_wire: 0,
                inner_method: 0,
                encrypted: false,
                gp_bit1: false,
                version_needed: 20,
                mtime: (0, 0),
                external_attr: 0o100644u32 << 16,
                md5_digest: None,
                sha256_digest: None,
                index_terms: None,
                facet_body: None,
            };
            if opts.index_names {
                indexed_names.push(e.name.clone());
            }
            zw.write_entry(e)?;
        }
    }

    // --index-names: emit one synthetic STORE entry after all user
    // entries carrying the MPH over the names we just wrote. Readers
    // that know about `.tzip/names.v1` pick it up; readers that don't
    // see a normal (small) ZIP entry.
    if opts.index_names && !indexed_names.is_empty() {
        let payload = crate::nameidx::build(
            &indexed_names.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        )?;
        let name_idx_entry = crate::zipwriter::EncodedEntry {
            index: u64::MAX,
            name: crate::nameidx::INDEX_ENTRY_NAME.to_string(),
            body: payload,
            uncompressed_size: 0, // set below
            crc32: 0,              // set below
            method_on_wire: 0,     // Store
            inner_method: 0,
            encrypted: false,
            gp_bit1: false,
            version_needed: 20,
            mtime: (0, 0),
            external_attr: 0o100644u32 << 16,
            md5_digest: None,
            sha256_digest: None,
            index_terms: None,
            facet_body: None,
        };
        // Fix up size / crc for the synthetic entry.
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&name_idx_entry.body);
        let crc = hasher.finalize();
        let body_len = name_idx_entry.body.len() as u64;
        let mut e = name_idx_entry;
        e.uncompressed_size = body_len;
        e.crc32 = crc;
        zw.write_entry(e)?;
    }

    profile(profile_on, "main_write_loop", t_mainloop);
    // 8. Join all
    let t_join = std::time::Instant::now();
    feeder
        .join()
        .map_err(|_| anyhow::anyhow!("feeder thread panicked"))??;
    for h in reader_handles {
        h.join().ok().transpose()?;
    }
    for h in cpu_handles {
        h.join().ok().transpose()?;
    }
    profile(profile_on, "join_workers", t_join);

    let t_finish = std::time::Instant::now();
    let inner = zw.finish()?;
    let mut buf_writer = inner;
    use std::io::{Seek, SeekFrom, Write};
    buf_writer.flush()?;
    profile(profile_on, "zw.finish+flush", t_finish);

    // Trim any unused preallocated tail so the archive's on-disk size
    // matches its logical size. Ignore errors — worst case the file uses
    // slightly more disk than st_size reports until next mount cycle.
    let t_trim = std::time::Instant::now();
    let actual_len = buf_writer.stream_position().unwrap_or(0);
    if actual_len > 0 {
        let file = buf_writer.get_mut();
        let _ = file.set_len(actual_len);
        let _ = file.seek(SeekFrom::End(0));
    }
    profile(profile_on, "set_len+trim", t_trim);

    progress.finish("done");
    if let Some(t) = tui_handle {
        t.finish();
    }
    profile(profile_on, "TOTAL", overall_start);
    Ok(())
}

/// Pick BufWriter capacity for the output based on filesystem info.
/// Phase 5c helper: sample the first N JSON-shaped files from the
/// input tree and derive a `ColumnarSchema` from them. Scanner uses
/// the walker's own output so filter semantics (globs, exclude,
/// basename-only) match what the encode pass will see. Only runs
/// when the user passed `--write-columnar` without an explicit
/// schema path; the schema-from-file path stays untouched.
fn infer_columnar_schema(opts: &Options) -> Result<crate::columnar::ColumnarSchema> {
    const SAMPLE_SIZE: usize = 128;
    let items = walker::walk(walker::WalkOpts {
        roots: &opts.paths,
        exclude: &opts.exclude,
        sort: false,
        walk_threads: opts.walk_jobs.max(1),
        basename_only: opts.basename_only,
    })?;
    let globs = &opts.index_terms;
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(SAMPLE_SIZE);
    for item in items {
        if bodies.len() >= SAMPLE_SIZE {
            break;
        }
        if !globs
            .iter()
            .any(|g| walker::is_excluded(&item.name_in_archive, std::slice::from_ref(g)))
        {
            continue;
        }
        if let Ok(b) = std::fs::read(&item.path) {
            bodies.push(b);
        }
    }
    if bodies.is_empty() {
        anyhow::bail!(
            "columnar auto-infer: no entries matched `--index-terms`. \
             Check the glob patterns, or pass `--columnar-schema SCHEMA.json` explicitly."
        );
    }
    let slices: Vec<&[u8]> = bodies.iter().map(|b| b.as_slice()).collect();
    crate::columnar::ColumnarSchema::infer(&slices)
}

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
    // --apfs-device drives its own reader; the FIEMAP auto-tune below (which
    // stats real filesystem paths) doesn't apply to in-volume paths.
    #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
    {
        if opts.apfs_device.is_some() {
            return opts;
        }
    }

    #[cfg(target_os = "linux")]
    {
        use crate::linux_raw::{device_traits, LinuxExtentSource};

        let first = opts
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let mut changes: Vec<String> = Vec::new();

        // Engage the extent-order reader automatically when the source lives
        // on seeking or removable media (rotational disk or USB stick) and the
        // filesystem actually supports FIEMAP. On NVMe the reorder is neutral,
        // so we leave it opt-in there to avoid surprising anyone.
        if !opts.user_flags.raw_block && !opts.raw_block {
            let (rotational, removable) = device_traits(&first);
            let seeky = rotational == Some(true) || removable == Some(true);
            if seeky && LinuxExtentSource::fiemap_supported(&first) {
                opts.raw_block = true;
                let why = if rotational == Some(true) {
                    "rotational"
                } else {
                    "removable"
                };
                changes.push(format!("raw_block=on ({why} media)"));
                // One forward sweep beats several competing ones on seeking
                // media — mirror the external-drive default.
                if !opts.user_flags.read_jobs && opts.read_jobs > 1 {
                    opts.read_jobs = 1;
                    changes.push("read_jobs=1".into());
                }
            }
        }

        if !changes.is_empty() && !opts.quiet {
            eprintln!(
                "tzip: auto-tuned defaults for Linux source ({}). Override with the same flag.",
                changes.join(", ")
            );
        }
        return opts;
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    {
        return opts;
    }

    #[cfg(target_os = "macos")]
    {
        use crate::usb_info::probe_path_fast;

        // Collect fs snapshots for every input + the output's parent dir.
        // The `_fast` variant skips `diskutil info` (~400 ms shell-out)
        // when the fs_type + mount point make a USB match impossible —
        // i.e. the vast majority of workloads on an internal APFS boot
        // volume. See `probe_one_impl` for the full decision table.
        let mut probes: Vec<crate::usb_info::SourceReport> = Vec::new();
        for p in &opts.paths {
            if let Ok(r) = probe_path_fast(p) {
                probes.push(r);
            }
        }
        let out_parent = opts
            .archive
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        if let Ok(r) = probe_path_fast(&out_parent) {
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
            // Auto-enable --raw-block when it's likely to win.
            // Requirements: raw-apfs feature compiled in, running as root
            // (needed to open /dev/rdiskN), and the user didn't explicitly
            // disable it. Threshold for a workload worth the ~14 s
            // metadata prefetch cost is a soft one — we can't cheaply
            // enumerate size upfront in streaming mode, so we always
            // enable when eligible. On a workload too small to be worth
            // the prefetch, the fallback path is fine anyway.
            #[cfg(feature = "raw-apfs")]
            {
                let is_root = unsafe { libc::geteuid() == 0 };
                if !opts.user_flags.raw_block && !opts.raw_block && is_root {
                    opts.raw_block = true;
                    changes.push("raw_block=on (root+APFS-on-USB)".into());
                }
            }
            // --raw-block bypasses VFS so keep_cache/dispatch_io don't
            // apply. Empirically 8 is the sweet spot on a typical USB
            // SSD even with a sharded block cache: 16 readers doubled
            // per-file `read` time (13 ms → 28 ms) — the drive itself
            // serializes beyond ~8 concurrent commands, so extra fds
            // add coordination cost without throughput. Sharded cache
            // is kept so a faster drive would scale further.
            #[cfg(feature = "raw-apfs")]
            let raw_block_active = opts.raw_block;
            #[cfg(not(feature = "raw-apfs"))]
            let raw_block_active = false;
            if raw_block_active {
                if !opts.user_flags.read_jobs {
                    let target = 8;
                    if opts.read_jobs != target {
                        opts.read_jobs = target;
                        changes.push(format!("read_jobs={}", target));
                    }
                }
            } else {
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
    raw: RawItem,
    method: Method,
    level: u32,
    password: Option<&str>,
    scratch: &mut Scratch,
    single_file_parallel: bool,
    want_md5: bool,
    want_sha256: bool,
    index_terms_match: bool,
    want_facet_body: bool,
) -> Result<EncodedEntry> {
    let RawItem { index, item, bytes } = raw;
    let uncompressed_size = bytes.len() as u64;

    // Fused plaintext pass: CRC-32 + optional MD5 + optional SHA-256
    // (+ optional term tokenization) in one sweep so each extra
    // digest or token-collect step is a near-free extension of the
    // CRC pass we're already paying for. `bytes` is already moved
    // out of `raw`; borrow it for each pass.
    let mut crc_h = crc32fast::Hasher::new();
    crc_h.update(&bytes);
    let crc32 = crc_h.finalize();
    let md5_digest: Option<[u8; 16]> = if want_md5 {
        use md5::Digest as _;
        let mut h = md5::Md5::new();
        h.update(&bytes[..]);
        let out: [u8; 16] = h.finalize().into();
        Some(out)
    } else {
        None
    };
    let sha256_digest: Option<[u8; 32]> = if want_sha256 {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        h.update(&bytes[..]);
        let out: [u8; 32] = h.finalize().into();
        Some(out)
    } else {
        None
    };
    let index_terms: Option<Vec<Vec<u8>>> = if index_terms_match {
        let mut set = std::collections::HashSet::new();
        crate::termidx::tokenize(&bytes[..], &mut set);
        Some(set.into_iter().collect())
    } else {
        None
    };
    // Snapshot plaintext for facet accumulation on the main thread.
    // Facet extraction is cross-entry (each facet accumulates across
    // every matching entry) and runs best serially, so the worker
    // just hands plaintext over.
    let facet_body: Option<Vec<u8>> = if want_facet_body {
        Some(bytes[..].to_vec())
    } else {
        None
    };

    // Store-method fast path: bypass `compress`'s per-call memcpy and
    // hand the input Vec straight through. For `Owned` reads this is
    // a zero-copy move; `Mapped` still needs a copy to materialise an
    // owned body for the writer. Combined, this halves memory traffic
    // on `-m store` for small-to-medium corpora.
    let out = if single_file_parallel {
        compress::compress_parallel_deflate(&bytes, level)?
    } else if matches!(method, Method::Store) {
        compress::CompressOut { data: bytes.into_vec(), gp_bit1: false }
    } else {
        compress::compress(method, level, &bytes, scratch)?
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
        index,
        name: item.name_in_archive,
        body,
        uncompressed_size,
        crc32,
        method_on_wire,
        inner_method,
        encrypted,
        gp_bit1: out.gp_bit1,
        version_needed,
        mtime: item.mtime,
        external_attr: item.external_attr,
        md5_digest,
        sha256_digest,
        index_terms,
        facet_body,
    })
}

