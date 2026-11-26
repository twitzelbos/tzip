//! Reader subcommands: `tzip ls`, `tzip cat`, `tzip x`.
//!
//! These form Increment 1 of the reader stack: everything operates on a
//! local file. Future increments (HTTP Range, S3, mount) swap in different
//! `Read + Seek` sources but reuse the same per-entry pipeline (locate
//! body range from CDR → stream decrypt → stream decompress).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::decompress::ReadMethod;
use crate::sources::{http::HttpSource, s3::S3Source};
use crate::zipreader::{method_from_id, stream_entry_source_to, Archive, ReadEntry};

/// Which reader op is running and what it needs. Lives here (not in
/// `cli`) so the reader module isn't forced to depend on `cli`,
/// which fed a long-standing module cycle.
#[derive(Clone, Debug)]
pub enum ReaderOp {
    Ls,
    Cat { name: String },
    Extract {
        to: PathBuf,
        names: Vec<String>,
        parallel: usize,
        connections: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub struct ReaderOptions {
    /// Local path, `http(s)://` URL, or `s3://bucket/key` URL.
    pub archive: String,
    pub password: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub op: ReaderOp,
    /// Phase-6 index cache spec. Threaded through from
    /// `--cache-dir` / `--cache-size-mb` / `--cache-ram-mb`;
    /// `None` = cache disabled. The reader runners open the
    /// cache once per invocation and pass the handle into
    /// `open_archive_with_cache` so repeat remote `ls`/`cat`/`x`
    /// against the same ETag hits the cached CDR + `.tzip/*`
    /// sentinels.
    pub cache: Option<crate::indexcache::IndexCacheSpec>,
}

/// Dispatches on scheme: `s3://…` → `S3Source`, `http(s)://…` → `HttpSource`,
/// anything else → local file path. Called by every reader subcommand so
/// local and remote behave identically downstream of this seam.
pub(crate) fn open_archive(spec: &str) -> Result<Archive> {
    open_archive_with_cache(spec, None)
}

/// Open the Phase-6 index cache if a spec was supplied,
/// otherwise return `None`. Called by `run_ls`/`run_cat`/
/// `run_extract` once per invocation; the resulting handle is
/// threaded into the archive open so `.tzip/*` sentinel and
/// CDR reads consult the cache. Local archives silently skip
/// the cache (no payoff: index reads are already a local file
/// seek, and the CDR is in the same file).
pub(crate) fn open_cache_if_configured(
    spec: Option<&crate::indexcache::IndexCacheSpec>,
) -> Result<Option<std::sync::Arc<crate::indexcache::IndexCache>>> {
    match spec {
        Some(s) => Ok(Some(s.open()?)),
        None => Ok(None),
    }
}

/// Same as `open_archive` but with an optional Phase-6 index cache.
/// Local archives ignore it (no payoff: index reads are already a
/// local file seek). Remote archives (http/https/s3) stash + return
/// the ETag / Last-Modified fingerprint captured at open time;
/// subsequent `.tzip/*.v1` loads consult the cache first.
pub(crate) fn open_archive_with_cache(
    spec: &str,
    cache: Option<std::sync::Arc<crate::indexcache::IndexCache>>,
) -> Result<Archive> {
    // Local paths go through `Archive::open` so multi-disk spanned
    // archives get stitched across their sibling `.z01`..`.zip` files.
    // Remote schemes (http/https/s3) don't have sibling disks — they
    // use `open_source_for` + `open_source`, which fails loudly on
    // multi-disk EOCDs rather than silently misinterpreting them.
    if spec.starts_with("s3://") {
        let rest = &spec["s3://".len()..];
        let mut src = crate::sources::s3::S3Source::open(rest)
            .with_context(|| format!("open S3 source s3://{}", rest))?;
        // Real ContentLength (via SeekFrom::End) as the
        // last-ditch fingerprint — much better than the
        // URL-string-length nonsense this used to pass.
        let len = src
            .seek(std::io::SeekFrom::End(0))
            .ok()
            .unwrap_or(0);
        let binding = cache_binding_for_remote(spec, src.fingerprint(), len, cache);
        Archive::open_source_with_cache(Box::new(src), binding)
    } else if spec.starts_with("http://") || spec.starts_with("https://") {
        let mut src = crate::sources::http::HttpSource::open(spec)
            .with_context(|| format!("open HTTP source {}", spec))?;
        let len = src
            .seek(std::io::SeekFrom::End(0))
            .ok()
            .unwrap_or(0);
        let binding = cache_binding_for_remote(src.url(), src.fingerprint(), len, cache);
        Archive::open_source_with_cache(Box::new(src), binding)
    } else {
        Archive::open(Path::new(spec))
    }
}

/// Build the (cache, archive-id) pair the Archive needs when a
/// cache is in scope.
///
/// Fingerprint priority (strongest first):
///
/// 1. `fingerprint` — the ETag the source captured at open
///    time, or `Last-Modified` if ETag was missing (the
///    source resolves the two internally).
/// 2. `weak_len` — the real `Content-Length` of the object,
///    so a changed object at the same URL with a different
///    size still misses the cache.
///
/// If **neither** is available (unusual: a source that
/// returned no ETag, no Last-Modified, *and* no
/// `Content-Length`), we **hard-disable the cache for this
/// archive** rather than silently keying on URL alone. A
/// URL-only key would happily serve stale bytes after a
/// same-URL object change, which defeats the invalidation
/// story Phase 6 + the CDR cache rely on. Security analysis
/// in `docs/scenarios/s3-huge-archive-single-entry.md`.
fn cache_binding_for_remote(
    url: &str,
    fingerprint: Option<&str>,
    weak_len: u64,
    cache: Option<std::sync::Arc<crate::indexcache::IndexCache>>,
) -> Option<(
    std::sync::Arc<crate::indexcache::IndexCache>,
    crate::indexcache::ArchiveId,
)> {
    let cache = cache?;
    let fp: String = match fingerprint {
        Some(s) => s.to_string(),
        None if weak_len > 0 => format!("len={}", weak_len),
        None => {
            // No fingerprint, no Content-Length — can't key
            // the cache safely. Disable rather than share.
            return None;
        }
    };
    Some((cache, crate::indexcache::ArchiveId::derive(url, &fp)))
}

/// Caller-known source hint. When the caller has already opened and
/// parsed the archive on the main thread, it already knows the disk
/// layout (local) or total byte length (remote). Passing that down to
/// per-worker `open_source_for_with_hint` lets each worker skip its own
/// EOCD scan (local) or length-probe HEAD/Range-0-0 round-trip
/// (HTTP/S3). `None` = unknown, probe.
#[derive(Clone, Copy)]
pub(crate) enum SourceHint {
    LocalSingleDisk,
    LocalMultiDisk(usize),
    RemoteKnownLen(u64),
}

/// Build a hint from an already-opened `Archive`. Workers spawned
/// after this point reuse the probe the main thread already did.
pub(crate) fn hint_from_archive(spec: &str, arch: &Archive) -> Option<SourceHint> {
    if spec.starts_with("http://") || spec.starts_with("https://") || spec.starts_with("s3://") {
        Some(SourceHint::RemoteKnownLen(arch.file_size()))
    } else if arch.disk_count() >= 2 {
        Some(SourceHint::LocalMultiDisk(arch.disk_count()))
    } else {
        Some(SourceHint::LocalSingleDisk)
    }
}

/// Open a raw `Read + Seek + Send` backing source for `spec` without
/// parsing the archive, taking a pre-probed shape hint so grep /
/// extract inner workers skip a second EOCD scan (local) or
/// length-probe HEAD/GET (remote). `None` = unknown, probe.
pub(crate) fn open_source_for_with_hint(
    spec: &str,
    hint: Option<SourceHint>,
) -> Result<Box<dyn crate::sources::ReadSeek>> {
    if let Some(rest) = spec.strip_prefix("s3://") {
        let src = match hint {
            Some(SourceHint::RemoteKnownLen(len)) => S3Source::open_with_len(rest, len),
            _ => S3Source::open(rest),
        }
        .with_context(|| format!("open S3 source s3://{}", rest))?;
        Ok(Box::new(src))
    } else if spec.starts_with("http://") || spec.starts_with("https://") {
        let src = match hint {
            Some(SourceHint::RemoteKnownLen(len)) => HttpSource::open_with_len(spec, len),
            _ => HttpSource::open(spec),
        }
        .with_context(|| format!("open HTTP source {}", spec))?;
        Ok(Box::new(src))
    } else {
        local_source_for(Path::new(spec), hint)
    }
}

fn local_source_for(
    path: &Path,
    hint: Option<SourceHint>,
) -> Result<Box<dyn crate::sources::ReadSeek>> {
    use crate::sources::multidisk::MultiDiskSource;

    // Fast path: caller told us the layout. Skip the EOCD scan.
    match hint {
        Some(SourceHint::LocalSingleDisk) => {
            let f = std::fs::File::open(path)
                .with_context(|| format!("open {}", path.display()))?;
            return Ok(Box::new(f));
        }
        Some(SourceHint::LocalMultiDisk(n)) => {
            let md = MultiDiskSource::open(path, n)
                .with_context(|| format!("open multi-disk source for {}", path.display()))?;
            return Ok(Box::new(md));
        }
        // RemoteKnownLen reached here only if a bad call site passed
        // a remote hint to a local path; fall through and probe.
        Some(SourceHint::RemoteKnownLen(_)) | None => {}
    }

    // No hint — scan EOCD once to decide. Keep the probe fd in-hand
    // for the single-disk outcome so this still costs a single open
    // overall (the old flow dropped and reopened).
    let mut probe = std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let file_size = probe
        .seek(SeekFrom::End(0))
        .with_context(|| format!("seek to end of {}", path.display()))?;
    let total_disks = crate::zipreader::probe_total_disks(&mut probe, file_size).ok();

    match total_disks {
        Some(n) if n >= 2 => {
            drop(probe);
            let md = MultiDiskSource::open(path, n)
                .with_context(|| format!("open multi-disk source for {}", path.display()))?;
            Ok(Box::new(md))
        }
        _ => Ok(Box::new(probe)),
    }
}

// ── Entry points ───────────────────────────────────────────────────────────

pub fn run_ls(opts: ReaderOptions) -> Result<()> {
    let cache = open_cache_if_configured(opts.cache.as_ref())?;
    let arch = open_archive_with_cache(&opts.archive, cache)
        .with_context(|| format!("open {}", opts.archive))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    writeln!(
        out,
        "  Length    Compressed  Method    Modified             Name"
    )?;
    writeln!(
        out,
        "----------  ----------  -------   ----------------  --  ----"
    )?;

    let mut total_uncomp = 0u64;
    let mut total_comp = 0u64;
    for e in arch.iter_entries() {
        let method = method_label(e);
        let encrypted_mark = if e.encrypted { "*" } else { " " };
        let dt = format_dos_mtime(e.mtime);
        writeln!(
            out,
            "{:>10}  {:>10}  {:<9} {}  {} {}",
            e.uncomp_size, e.comp_size, method, dt, encrypted_mark, e.name,
        )?;
        if opts.verbose && !e.comment.is_empty() {
            writeln!(out, "            # {}", e.comment)?;
        }
        total_uncomp += e.uncomp_size;
        total_comp += e.comp_size;
    }
    writeln!(
        out,
        "----------  ----------                                   -------"
    )?;
    writeln!(
        out,
        "{:>10}  {:>10}                                    {} files",
        total_uncomp,
        total_comp,
        arch.len()
    )?;
    if !arch.archive_comment().is_empty() {
        writeln!(out)?;
        writeln!(out, "Archive comment:")?;
        for line in arch.archive_comment().lines() {
            writeln!(out, "  {}", line)?;
        }
    }
    Ok(())
}

pub fn run_cat(opts: ReaderOptions) -> Result<()> {
    let name = match &opts.op {
        ReaderOp::Cat { name } => name.clone(),
        _ => unreachable!("run_cat called with wrong op"),
    };
    let cache = open_cache_if_configured(opts.cache.as_ref())?;
    let mut arch = open_archive_with_cache(&opts.archive, cache)
        .with_context(|| format!("open {}", opts.archive))?;
    if let Some(pw) = opts.password.as_deref() {
        arch.attach_password(pw.to_string());
    }

    let idx = find_entry_index(&arch, &name)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    arch.stream_entry_to(idx, opts.password.as_deref(), &mut out)?;
    Ok(())
}

pub fn run_extract(opts: ReaderOptions) -> Result<()> {
    let profile_on = std::env::var("TZIP_PROFILE").ok().as_deref() == Some("1");
    let overall_start = std::time::Instant::now();
    let p = |stage: &str, start: std::time::Instant| {
        if profile_on {
            eprintln!(
                "tzip profile: {:22} {:>8.3} ms",
                stage,
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    };

    let (to, names_filter, parallel_req, connections) = match &opts.op {
        ReaderOp::Extract {
            to,
            names,
            parallel,
            connections,
        } => (to.clone(), names.clone(), *parallel, *connections),
        _ => unreachable!("run_extract called with wrong op"),
    };

    let t = std::time::Instant::now();
    fs::create_dir_all(&to)
        .with_context(|| format!("create extraction root {}", to.display()))?;
    let to = fs::canonicalize(&to)
        .with_context(|| format!("canonicalize {}", to.display()))?;
    p("mkdir+canonicalize", t);

    // `--connections` caps worker count so you can bound an HTTP pool to a
    // size smaller than `-P`. On local archives the two flags collapse.
    let parallel = match connections {
        Some(c) => parallel_req.min(c.max(1)),
        None => parallel_req,
    }
    .max(1);

    let cache = open_cache_if_configured(opts.cache.as_ref())?;
    let t = std::time::Instant::now();
    let mut arch = open_archive_with_cache(&opts.archive, cache)
        .with_context(|| format!("open {}", opts.archive))?;
    if let Some(pw) = opts.password.as_deref() {
        arch.attach_password(pw.to_string());
    }
    p("open_archive", t);

    let selected_set: Option<std::collections::HashSet<String>> = if names_filter.is_empty() {
        None
    } else {
        Some(names_filter.iter().cloned().collect())
    };

    // Fail fast on typo'd names before we start writing anything.
    if let Some(sel) = &selected_set {
        let present: std::collections::HashSet<&str> =
            arch.iter_entries().map(|e| e.name.as_str()).collect();
        let missing: Vec<&String> = sel
            .iter()
            .filter(|n| !present.contains(n.as_str()))
            .collect();
        if !missing.is_empty() {
            bail!(
                "entries not in archive: {}",
                missing
                    .iter()
                    .map(|s| format!("{:?}", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // Phase A (serial, needs &mut arch): populate lfh_total_size and clone
    // the selected entries so the parallel workers get owned, self-contained
    // metadata and never touch `arch`.
    let selected_idx: Vec<usize> = (0..arch.len())
        .filter(|&idx| match &selected_set {
            None => true,
            Some(sel) => sel.contains(&arch.entry(idx).name),
        })
        .collect();
    let t = std::time::Instant::now();
    let mut selected: Vec<ReadEntry> = Vec::with_capacity(selected_idx.len());
    for idx in selected_idx {
        arch.lfh_total_size_of(idx)?;
        selected.push(arch.entry(idx).clone());
    }
    // Build a worker-side source hint from `arch` before dropping it —
    // disk count for local archives, total byte length for HTTP/S3.
    // Workers then skip their EOCD scan (local) or length-probe
    // HEAD/Range-0-0 round-trip (remote).
    let worker_hint = hint_from_archive(&opts.archive, &arch);
    drop(arch);
    p("lfh_probe_phase_A", t);

    // Phase B (serial): resolve on-disk destinations, create every parent
    // directory upfront (dedup'd via BTreeSet) so file-writing workers
    // never race on `mkdir`. Entries that are purely directories (name
    // ends in `/`, zero body) are created here too and don't enter the
    // worker queue.
    let mut tasks: Vec<(ReadEntry, PathBuf)> = Vec::with_capacity(selected.len());
    let mut explicit_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut mkdir_set: BTreeSet<PathBuf> = BTreeSet::new();
    for re in selected {
        let dest = safe_join(&to, &re.name)
            .with_context(|| format!("entry name {:?}", re.name))?;
        if re.name.ends_with('/') && re.uncomp_size == 0 {
            mkdir_set.insert(dest.clone());
            explicit_dirs.insert(dest);
            continue;
        }
        if let Some(parent) = dest.parent() {
            mkdir_set.insert(parent.to_path_buf());
        }
        tasks.push((re, dest));
    }
    let t = std::time::Instant::now();
    for d in &mkdir_set {
        fs::create_dir_all(d)
            .with_context(|| format!("mkdir {}", d.display()))?;
    }
    p("mkdir_parents_phase_B", t);

    // Phase C (parallel): fan-out file extractions. Each worker holds its
    // own backing source so HTTP/S3 Range GETs overlap. Unbounded channel
    // is intentional — the task list is already materialised, and a
    // bounded buffer would deadlock on `total_files > buffer` because we
    // send all tasks before spawning workers.
    let total_files = tasks.len();
    let (tx, rx) = crossbeam_channel::unbounded::<(ReadEntry, PathBuf)>();
    for t in tasks {
        if tx.send(t).is_err() {
            break;
        }
    }
    drop(tx);

    let password = opts.password.clone();
    let spec = opts.archive.clone();
    let verbose = opts.verbose;
    let extracted = AtomicUsize::new(0);

    // Precompute the mode a freshly-created file will land with so
    // workers can skip `fchmod` whenever the archived mode already
    // matches. Reading the umask is a per-process syscall pair that
    // isn't thread-safe (umask is process-global); doing it once on
    // the main thread avoids all that.
    let default_mode = created_file_default_mode();


    let t_fanout = std::time::Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::with_capacity(parallel);
        let workers = parallel.min(total_files.max(1));
        for _ in 0..workers {
            let rx = rx.clone();
            let password = password.clone();
            let spec = spec.clone();
            let extracted = &extracted;
            let handle = scope.spawn(move || -> Result<()> {
                let mut source = open_source_for_with_hint(&spec, worker_hint)
                    .with_context(|| format!("open worker source for {}", spec))?;
                // Per-worker cache of parent-directory fds. For a flat
                // extract, one cached fd handles every entry — turning
                // the per-entry `File::create(absolute_path)` into
                // `openat(cached_dir_fd, basename)` and skipping the
                // kernel's path-component walk from the mount point
                // down to the parent. Small LRU covers nested archives.
                let mut dirfd_cache =
                    crate::dirfd_cache::DirFdCache::new(crate::dirfd_cache::DEFAULT_DIRFD_CAP);
                while let Ok((re, dest)) = rx.recv() {
                    let write_result = (|| -> Result<()> {
                        let f = dirfd_cache
                            .create_at(&dest)
                            .with_context(|| format!("create {}", dest.display()))?;
                        // 64 KiB buffer: most zip entries are small enough
                        // that one or two writev syscalls per file is
                        // enough to drain. The default 8 KiB costs ~6
                        // syscalls on a 50 KiB entry.
                        let mut buf = BufWriter::with_capacity(64 * 1024, f);
                        stream_entry_source_to(&mut *source, &re, password.as_deref(), &mut buf)?;
                        buf.flush().context("flush extract buffer")?;
                        // Hold the handle open for the metadata ops so we
                        // avoid a second `open`+`close` per entry. On APFS
                        // each path-based `open` is ~0.5-1 ms; re-opening
                        // just to call `set_modified` was the dominant
                        // per-entry cost.
                        let f = buf.into_inner().context("unwrap extract buffer")?;
                        apply_metadata(&f, &re, default_mode);
                        Ok(())
                    })();
                    match write_result {
                        Ok(()) => {
                            if verbose {
                                eprintln!("extracted {}", re.name);
                            }
                            extracted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&dest);
                            return Err(e).with_context(|| format!("extract {}", re.name));
                        }
                    }
                }
                Ok(())
            });
            handles.push(handle);
        }
        let mut first_err: Option<anyhow::Error> = None;
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(anyhow!("extract worker thread panicked"));
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })?;

    p("fanout_phase_C", t_fanout);

    let file_count = extracted.into_inner();
    let total = file_count + explicit_dirs.len();
    if !opts.quiet {
        eprintln!("extracted {} entries to {}", total, to.display());
    }
    p("TOTAL", overall_start);
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────

/// What mode a newly-created regular file will land with on this
/// process. Read the current umask by temporarily setting it to 0
/// and restoring; the window is a single syscall pair on the main
/// thread before any worker spawns, so no one else is racing. On
/// non-unix hosts there's no umask — return 0 so the comparison in
/// `apply_metadata` never short-circuits.
#[cfg(unix)]
fn created_file_default_mode() -> u32 {
    // SAFETY: libc::umask is always safe to call; the only risk is the
    // brief process-global umask=0 window, which we close immediately
    // and before any file-creating thread exists.
    let prev = unsafe {
        let zero = libc::umask(0);
        libc::umask(zero);
        zero
    };
    0o666 & !(prev as u32)
}

#[cfg(not(unix))]
fn created_file_default_mode() -> u32 {
    0
}

/// Apply the entry's permission bits and mtime via the already-open
/// `File` handle. One `fchmod` + one `futimens`, both on the fd we
/// already have, with the `fchmod` elided when the archived mode
/// matches what `File::create` already produced (common case: text
/// files at mode 0o644). Best-effort: failures are silently ignored.
/// The extract itself has already succeeded by the time we're here.
///
/// `default_mode` is the mode newly-created files would receive under
/// the current process umask, precomputed on the main thread before
/// workers spawn so we avoid a per-thread umask() dance.
fn apply_metadata(f: &File, re: &ReadEntry, _default_mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = re.unix_mode() {
            // Only the file-permission bits (setuid/setgid/sticky +
            // rwxrwxrwx); type bits are dropped. A zero mode means
            // the archive didn't carry Unix perms, so we leave the
            // default from `File::create`.
            let perm_bits = mode & 0o7777;
            if perm_bits != 0 && perm_bits != _default_mode {
                let _ = f.set_permissions(std::fs::Permissions::from_mode(perm_bits));
            }
        }
    }
    // Windows: the DOS external_attr bits are a different beast; leave
    // whatever ACL File::create produced. The ZIP spec defers to the
    // platform on "how to restore attributes" — doing nothing is fine.

    let ts = crate::dos_time::dos_to_unix(re.mtime);
    if ts > 0 {
        use std::time::{Duration, UNIX_EPOCH};
        let st = UNIX_EPOCH + Duration::from_secs(ts as u64);
        let _ = f.set_modified(st);
    }
}

fn find_entry_index(arch: &Archive, name: &str) -> Result<usize> {
    // `lookup_entry_idx` is O(1) when the archive carries a
    // `.tzip/names.v1` MPH index; it falls back to the same linear
    // scan this function used to do when no index is present.
    arch.lookup_entry_idx(name)
        .ok_or_else(|| anyhow!("no entry named {:?} in archive", name))
}

fn method_label(e: &ReadEntry) -> &'static str {
    match method_from_id(e.inner_method, e.gp_flags) {
        Some(m) => m.label(),
        None => "?",
    }
}


fn format_dos_mtime((date, time): (u16, u16)) -> String {
    // DOS date = (year-1980) << 9 | month << 5 | day
    // DOS time = hour << 11 | minute << 5 | sec/2
    let year = 1980 + ((date >> 9) & 0x7F) as u32;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as u32;
    let minute = ((time >> 5) & 0x3F) as u32;
    let sec = ((time & 0x1F) << 1) as u32;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hour, minute, sec
    )
}

/// Resolve an entry's archive path to a filesystem path under `root`,
/// rejecting zip-slip patterns (`..`, absolute paths, Windows drive
/// letters, UNC paths). Normalized via lexical join only — no filesystem
/// lookup that could follow a symlink out of `root`.
fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    // Reject absolute paths, drive letters, UNC prefixes outright.
    if name.starts_with('/') || name.starts_with('\\') {
        bail!("entry name is absolute: {:?}", name);
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        bail!("entry name looks like a Windows drive path: {:?}", name);
    }

    let mut out = root.to_path_buf();
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            bail!("entry name contains `..`: {:?}", name);
        }
        // Also reject components that would resolve to anything parent-
        // ish when the join happens.
        let p = Path::new(seg);
        for c in p.components() {
            match c {
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("entry segment is unsafe: {:?}", seg);
                }
                Component::CurDir => {}
                Component::Normal(_) => {}
            }
        }
        out.push(seg);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ZIP method ID → `ReadMethod` mapping. Covers the modern
    /// tzip-written methods *and* the pre-Deflate PKZIP methods so a
    /// future ID change (`inner_method` field widening, say) fails
    /// loudly instead of silently rerouting Imploded entries to Store.
    #[test]
    fn method_from_id_covers_all_supported() {
        assert!(matches!(method_from_id(0, 0), Some(ReadMethod::Store)));
        assert!(matches!(method_from_id(1, 0), Some(ReadMethod::Shrink)));
        assert!(matches!(method_from_id(2, 0), Some(ReadMethod::Reduce(1))));
        assert!(matches!(method_from_id(3, 0), Some(ReadMethod::Reduce(2))));
        assert!(matches!(method_from_id(4, 0), Some(ReadMethod::Reduce(3))));
        assert!(matches!(method_from_id(5, 0), Some(ReadMethod::Reduce(4))));
        // Implode with no flags → small window, two trees.
        match method_from_id(6, 0) {
            Some(ReadMethod::Implode { large_window, lit_tree }) => {
                assert!(!large_window);
                assert!(!lit_tree);
            }
            other => panic!("expected Implode, got {:?}", other),
        }
        // Implode with GP bits 1 and 2 → large window, literal tree.
        match method_from_id(6, 0b0000_0110) {
            Some(ReadMethod::Implode { large_window, lit_tree }) => {
                assert!(large_window);
                assert!(lit_tree);
            }
            other => panic!("expected Implode(large,lit), got {:?}", other),
        }
        assert!(matches!(method_from_id(8, 0), Some(ReadMethod::Deflate)));
        assert!(matches!(method_from_id(9, 0), Some(ReadMethod::Deflate64)));
        assert!(matches!(method_from_id(12, 0), Some(ReadMethod::Bzip2)));
        assert!(matches!(method_from_id(14, 0), Some(ReadMethod::Lzma)));
        assert!(matches!(method_from_id(93, 0), Some(ReadMethod::Zstd)));
        assert!(matches!(method_from_id(95, 0), Some(ReadMethod::Xz)));
        assert!(matches!(method_from_id(98, 0), Some(ReadMethod::Ppmd)));
        // Reserved / unknown method IDs stay unsupported.
        assert!(method_from_id(7, 0).is_none());
        assert!(method_from_id(10, 0).is_none());
        assert!(method_from_id(97, 0).is_none());
        assert!(method_from_id(99, 0).is_none());
    }
}
