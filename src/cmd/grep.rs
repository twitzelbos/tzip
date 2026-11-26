//! `tzip grep PATTERN <archive>...` — fan-out regex search across many
//! archives (local, HTTP, S3). The outer loop parallelises across
//! archives at `-P`; within each archive, inner workers parallelise
//! across entries at `--inner-jobs`, each holding its own backing source
//! (= its own HTTP/S3 connection) so Range GETs can overlap.
//!
//! The actual regex matching uses the `regex` crate — the exact engine
//! ripgrep ships, factored into a reusable library. For users who want
//! ripgrep's full feature set (PCRE2, `-C`, colored output), `--exec`
//! writes each candidate entry to a tempfile and delegates to the
//! external command.
//!
//! CDR-side filters (`--include` / `--exclude` / `--min-size` /
//! `--max-size` / `--since` / `--until`) are applied *before* fetching
//! any entry body — the big win on remote sources where every skipped
//! entry is a saved Range GET.
//!
//! Progress + crash safety via `--checkpoint`: one JSONL line per
//! completed `(archive, entry)` pair. `--resume` picks up where the
//! previous run left off and refuses mismatched queries.

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Sender;
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::cli::GrepOptions;
use crate::reader::open_archive_with_cache;
use crate::zipreader::{stream_entry_source_to, ReadEntry};

const CHECKPOINT_VERSION: u32 = 1;

pub fn run(opts: GrepOptions) -> Result<()> {
    // 1. Expand the archive list: locals pass through, s3://…/*.zip
    //    goes through ListObjectsV2.
    let archives = crate::shards::expand(&opts.archives)?;
    if archives.is_empty() {
        if !opts.quiet {
            eprintln!("tzip: no archives matched the given patterns");
        }
        return Ok(());
    }
    if opts.verbose {
        eprintln!("tzip: grepping {} archive(s)", archives.len());
    }

    // 2. Build regex once; shared across workers behind an Arc.
    let regex = Arc::new(build_regex(&opts.pattern, opts.ignore_case)?);

    // 3. Load / initialise checkpoint.
    let checkpoint = checkpoint_init(&opts, &archives)?;
    let skip = checkpoint
        .as_ref()
        .map(|cp| cp.skip.clone())
        .unwrap_or_default();
    let cp_writer: Option<Arc<Mutex<File>>> = checkpoint.as_ref().map(|cp| cp.writer.clone());

    // 4. Build the Phase-6 index cache once per invocation. Shared
    //    across all archive workers via Arc — concurrent `tzip grep`s
    //    against the same URL set share the same on-disk cache.
    let cache = match &opts.cache {
        Some(spec) => Some(spec.open()?),
        None => None,
    };

    // 5. Outer fan-out: archives in parallel.
    let outer = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.parallel)
        .thread_name(|i| format!("tzip-grep-outer-{}", i))
        .build()
        .context("build outer rayon pool")?;
    let shared_out: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let opts = Arc::new(opts);
    let skip = Arc::new(skip);

    outer.install(|| -> Result<()> {
        use rayon::prelude::*;
        archives
            .par_iter()
            .try_for_each(|url| -> Result<()> {
                process_archive(
                    url,
                    Arc::clone(&regex),
                    Arc::clone(&opts),
                    Arc::clone(&skip),
                    cp_writer.clone(),
                    Arc::clone(&shared_out),
                    cache.clone(),
                )
            })
    })?;

    Ok(())
}

// ── Per-archive worker ─────────────────────────────────────────────────────

fn process_archive(
    url: &str,
    regex: Arc<Regex>,
    opts: Arc<GrepOptions>,
    skip: Arc<HashSet<(String, String)>>,
    cp_writer: Option<Arc<Mutex<File>>>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    cache: Option<Arc<crate::indexcache::IndexCache>>,
) -> Result<()> {
    // Phase 1: parse CDR, filter entries, populate lfh_total_size.
    let mut driver = open_archive_with_cache(url, cache)
        .with_context(|| format!("open archive {}", url))?;
    // Attach password so the term-index sentinel (`.tzip/terms.v1`)
    // can be decrypted on AES-encrypted archives. Without this,
    // `term_candidates` returns None and grep degenerates to a
    // full linear scan of every entry.
    if let Some(pw) = opts.password.as_deref() {
        driver.attach_password(pw.to_string());
    }
    // Narrow the global `(url, name)` skip set to the entries we'd
    // actually probe on THIS archive, as a `HashSet<&str>` keyed by
    // name alone. Lets the hot `.contains(...)` check be zero-alloc —
    // the old `skip.contains(&(url.to_string(), name.clone()))`
    // allocated two Strings per candidate.
    let url_skip: HashSet<&str> = skip
        .iter()
        .filter_map(|(u, n)| (u == url).then(|| n.as_str()))
        .collect();
    // If the archive carries a `.tzip/terms.v1` index AND the user's
    // regex pattern has an extractable literal set of tokens (a plain
    // literal string with no regex metacharacters), consult the
    // index to narrow the candidate set. Everything else falls
    // through to full linear scan.
    let profile_on = std::env::var("TZIP_PROFILE").ok().as_deref() == Some("1");
    let t_term = std::time::Instant::now();
    let term_candidates: Option<std::collections::HashSet<u32>> =
        if is_plain_literal(&opts.pattern) {
            match driver.term_candidates(&opts.pattern) {
                Some(Some(set)) => Some(set),
                _ => None,
            }
        } else {
            None
        };
    if profile_on {
        eprintln!(
            "tzip grep profile: term_candidates = {} entries in {:.1} ms (pattern={:?} plain_literal={})",
            term_candidates.as_ref().map(|s| s.len()).unwrap_or(0),
            t_term.elapsed().as_secs_f64() * 1000.0,
            opts.pattern,
            is_plain_literal(&opts.pattern),
        );
    }

    // Collect candidate indices first so we can mutably probe LFH sizes
    // without holding an immutable borrow of the driver.
    //
    // Term-index narrowing (ChatGPT review #1 fix): when the index
    // reports `idx` isn't in the candidate set, we only skip the
    // entry if the index actually *covered* it at build time.
    // Entries outside the Phase-2 `--index-terms` glob (e.g. `.log`
    // in a `--index-terms '*.json'` corpus) are `TermCoverage::NotCovered`
    // and must be scanned conservatively — otherwise literal
    // matches in those entries disappear. Legacy archives
    // (`TermCoverage::Unknown`) likewise get scanned.
    // Two passes: first prefilter immutably on name / globs /
    // term-index membership, then — for the suspect "not in
    // candidate set" survivors only — take a mutable borrow to
    // ask `term_coverage` whether the index actually saw them.
    // Splitting the borrows lets `term_candidates` and
    // `term_coverage` stay `&mut self` (they lazy-load the
    // segments on first call; ChatGPT review #5) without
    // forcing a `filter` closure to hold two borrows at once.
    let mut maybe_candidates: Vec<usize> = Vec::new();
    for idx in 0..driver.len() {
        let re = driver.entry(idx);
        if !entry_passes_filters(re, &opts) || url_skip.contains(re.name.as_str()) {
            continue;
        }
        maybe_candidates.push(idx);
    }
    let t_filter = std::time::Instant::now();
    let mut dropped_by_index = 0usize;
    let mut kept_by_index_hit = 0usize;
    let mut kept_by_unknown_coverage = 0usize;
    let candidates: Vec<usize> = if let Some(ref set) = term_candidates {
        let mut kept = Vec::with_capacity(maybe_candidates.len());
        for idx in maybe_candidates {
            if set.contains(&(idx as u32)) {
                kept.push(idx);
                kept_by_index_hit += 1;
                continue;
            }
            // Candidate set says "not a match." Only trust that
            // when the index actually covered this entry;
            // otherwise fall through to scan.
            match driver.term_coverage(idx as u32) {
                crate::zipreader::TermCoverage::Covered => {
                    dropped_by_index += 1;
                }
                crate::zipreader::TermCoverage::NotCovered
                | crate::zipreader::TermCoverage::Unknown => {
                    kept.push(idx);
                    kept_by_unknown_coverage += 1;
                }
            }
        }
        kept
    } else {
        maybe_candidates
    };
    if profile_on {
        eprintln!(
            "tzip grep profile: filter in {:.1} ms — {} candidates \
             ({} index-hit, {} unknown-coverage, {} dropped-by-index)",
            t_filter.elapsed().as_secs_f64() * 1000.0,
            candidates.len(),
            kept_by_index_hit,
            kept_by_unknown_coverage,
            dropped_by_index,
        );
    }
    let mut filtered: Vec<ReadEntry> = Vec::with_capacity(candidates.len());
    for idx in candidates {
        let name = driver.entry(idx).name.clone();
        driver
            .lfh_total_size_of(idx)
            .with_context(|| format!("probe LFH for {} in {}", name, url))?;
        filtered.push(driver.entry(idx).clone());
    }
    // Sort by `lfh_offset` ascending so each worker's pulls
    // from the channel land as a mostly-forward sweep through
    // the archive. The S3Source caches the last ~1 MiB range
    // GET, so adjacent-offset scans hit the cache instead of
    // kicking off a fresh signed GET per entry. On a grep
    // with 100 hits packed near each other, this collapses
    // 100 Range GETs to ≈ 1 — the win the user flagged about
    // "every byte that needs to be transferred costs a lot."
    filtered.sort_by_key(|re| re.lfh_offset);
    // Build the worker-side source hint from the Archive we already
    // opened. Workers then skip the EOCD scan (local) or length-probe
    // HEAD / signed Range-0-0 round-trip (http/s3) they'd otherwise
    // each perform — same hoist we already do in `tzip x`.
    let worker_hint = crate::reader::hint_from_archive(url, &driver);
    drop(driver);
    if filtered.is_empty() {
        return Ok(());
    }

    // Phase 2: inner fan-out.
    //
    // `unbounded` by design: the full candidate list is already
    // sitting in `filtered` as a `Vec<ReadEntry>` — we're not
    // streaming from a live producer. The previous
    // `bounded(inner_jobs * 2)` was a deadlock trap (issue #54):
    // we pre-populated the channel *before* spawning any workers,
    // so with more than `inner_jobs * 2` candidates the producer
    // loop blocked on `tx.send` forever with nothing to drain it.
    // Threshold on the reproduce was ~100 entries at the default
    // `--inner-jobs 8`, which also made the bug invisible on every
    // existing test (all archives under 50 entries).
    //
    // Backpressure isn't needed here because the Vec is the
    // backpressure: every ReadEntry is already materialised and
    // moving it into the channel is a pointer copy, not an alloc.
    let (tx, rx) = crossbeam_channel::unbounded::<ReadEntry>();
    for re in filtered {
        if tx.send(re).is_err() {
            break;
        }
    }
    drop(tx);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(opts.inner_jobs);
        for _ in 0..opts.inner_jobs {
            let rx = rx.clone();
            let regex = Arc::clone(&regex);
            let opts = Arc::clone(&opts);
            let cp_writer = cp_writer.clone();
            let out = Arc::clone(&out);
            let url = url.to_string();
            let worker_hint = worker_hint.clone();
            let handle = scope.spawn(move || -> Result<()> {
                let mut source = crate::reader::open_source_for_with_hint(&url, worker_hint)
                    .with_context(|| format!("open inner source for {}", url))?;
                while let Ok(re) = rx.recv() {
                    // Per-entry errors (transient DNS, a
                    // single corrupt entry, a bad password on
                    // one entry out of thousands, …) should
                    // not kill the whole grep — matches
                    // already emitted are useful, and a
                    // lone network flake shouldn't erase them.
                    // Log to stderr, keep scanning the rest.
                    // The worker itself still returns Ok;
                    // only `open_source_for_with_hint` above
                    // (which is a one-shot at worker start)
                    // can bail the whole thread.
                    if let Err(e) =
                        scan_entry(&url, &mut *source, &re, &regex, &opts, cp_writer.as_ref(), &out)
                    {
                        eprintln!("tzip grep: {} — {:#}", re.name, e);
                    }
                }
                Ok(())
            });
            handles.push(handle);
        }
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(anyhow!("grep worker thread panicked")),
            }
        }
        Ok(())
    })
}

fn scan_entry(
    url: &str,
    source: &mut dyn crate::sources::ReadSeek,
    re: &ReadEntry,
    regex: &Regex,
    opts: &GrepOptions,
    cp_writer: Option<&Arc<Mutex<File>>>,
    out: &Arc<Mutex<Box<dyn Write + Send>>>,
) -> Result<()> {
    // For --exec we tee decompress output straight to a tempfile as it
    // comes out, so a 10 GB entry costs ~1 line of RAM in the scanner
    // + whatever decompress needs internally, not 10 GB.
    let mut exec_tmp: Option<tempfile::NamedTempFile> = if opts.exec.is_some() {
        Some(tempfile::NamedTempFile::new().context("create exec tempfile")?)
    } else {
        None
    };

    // files_only can short-circuit only when --exec is absent — under
    // --exec we need the full match_count for the checkpoint.
    let stop_after_first = opts.files_only && opts.exec.is_none();
    let emit_ctx = if opts.exec.is_none() {
        Some(EmitCtx { out, opts, archive: url, entry: &re.name })
    } else {
        None
    };

    let match_count: u64;
    {
        let tee: Option<&mut File> = exec_tmp.as_mut().map(|f| f.as_file_mut());
        let mut scanner = LineScanner::new(regex, tee, emit_ctx, stop_after_first);
        stream_entry_source_to(source, re, opts.password.as_deref(), &mut scanner)
            .with_context(|| format!("read body of {} in {}", re.name, url))?;
        match_count = scanner.match_count();
        scanner
            .finish()
            .with_context(|| format!("finalise scan of {} in {}", re.name, url))?;
    }

    if let Some(cmd) = &opts.exec {
        if match_count > 0 {
            let tmp = exec_tmp
                .take()
                .expect("exec_tmp is Some whenever opts.exec is Some");
            tmp.as_file().sync_data().ok();
            let cmd_rendered = cmd.replace("{}", &tmp.path().to_string_lossy());
            {
                let mut out = out.lock().unwrap();
                writeln!(out, "::: {}:{} :::", url, re.name).ok();
            }
            Command::new("sh")
                .arg("-c")
                .arg(&cmd_rendered)
                .status()
                .with_context(|| format!("spawn --exec command: {}", cmd_rendered))?;
            // Explicit drop keeps the tempfile alive until here.
            drop(tmp);
        }
    }

    // Checkpoint the completion of this (archive, entry).
    if let Some(w) = cp_writer {
        let rec = CheckpointEntry {
            archive: url.to_string(),
            entry: re.name.clone(),
            matches: match_count,
            ts: unix_now(),
        };
        let line = serde_json::to_string(&rec).context("serialize checkpoint entry")?;
        let mut f = w.lock().unwrap();
        writeln!(f, "{}", line).context("write checkpoint entry")?;
        f.sync_data().ok();
    }
    Ok(())
}

/// Context the scanner uses to emit a match line. Absent when `--exec`
/// is in effect — then matches drive whether the command runs, not
/// what's written to stdout.
struct EmitCtx<'a> {
    out: &'a Arc<Mutex<Box<dyn Write + Send>>>,
    opts: &'a GrepOptions,
    archive: &'a str,
    entry: &'a str,
}

/// A `Write` that scans decompress output chunk-by-chunk, keeping only
/// the trailing partial line in memory. Optional tee to a tempfile for
/// `--exec`. The final line (no trailing `\n`) is scanned by `finish()`
/// after the stream closes.
struct LineScanner<'a, T: Write> {
    regex: &'a Regex,
    tee: Option<T>,
    emit: Option<EmitCtx<'a>>,
    stop_after_first: bool,

    buf: Vec<u8>,
    line_no: u64,
    match_count: u64,
    halted: bool,
    /// Errors raised inside `write()` by the emit callback can't travel
    /// as `anyhow::Error` through the `io::Write` contract; stash them
    /// here and surface after `stream_entry_to` returns.
    deferred_err: Option<anyhow::Error>,
}

impl<'a, T: Write> LineScanner<'a, T> {
    fn new(
        regex: &'a Regex,
        tee: Option<T>,
        emit: Option<EmitCtx<'a>>,
        stop_after_first: bool,
    ) -> Self {
        Self {
            regex,
            tee,
            emit,
            stop_after_first,
            buf: Vec::new(),
            line_no: 0,
            match_count: 0,
            halted: false,
            deferred_err: None,
        }
    }

    /// Scan the final line (one without a trailing `\n`), if any,
    /// and surface any emit-side error the Write impl had to defer.
    /// The io::Write trait signature forces `scan_line`'s anyhow
    /// errors to be buffered in `deferred_err`; `finish` is where we
    /// unpack them so callers see one unified `Result`.
    fn finish(mut self) -> Result<()> {
        if let Some(t) = self.tee.as_mut() {
            t.flush().context("flush exec tempfile")?;
        }
        if !self.halted && !self.buf.is_empty() {
            self.line_no += 1;
            let line = std::mem::take(&mut self.buf);
            if self.regex.is_match(&line) {
                self.match_count += 1;
                if let Some(ctx) = &self.emit {
                    emit_match(ctx.out, ctx.opts, ctx.archive, ctx.entry, self.line_no, &line)?;
                }
            }
        }
        if let Some(err) = self.deferred_err.take() {
            return Err(err);
        }
        Ok(())
    }

    fn match_count(&self) -> u64 {
        self.match_count
    }

    /// Scan one completed line (buf + chunk-slice). Returns Err to
    /// defer anyhow errors — see `deferred_err`.
    fn scan_line(&mut self, chunk_slice: &[u8]) -> Result<()> {
        self.line_no += 1;
        // If `buf` is empty we can match on `chunk_slice` directly; this
        // is the common case and skips a vec alloc per line.
        let is_hit = if self.buf.is_empty() {
            self.regex.is_match(chunk_slice)
        } else {
            self.buf.extend_from_slice(chunk_slice);
            self.regex.is_match(&self.buf)
        };
        if !is_hit {
            self.buf.clear();
            return Ok(());
        }
        self.match_count += 1;
        if let Some(ctx) = &self.emit {
            let line_bytes: &[u8] = if self.buf.is_empty() { chunk_slice } else { &self.buf };
            emit_match(ctx.out, ctx.opts, ctx.archive, ctx.entry, self.line_no, line_bytes)?;
            if self.stop_after_first {
                self.halted = true;
            }
        }
        self.buf.clear();
        Ok(())
    }
}

impl<'a, T: Write> Write for LineScanner<'a, T> {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        // Tee first so --exec always sees the full body regardless of
        // whether the scanner has halted on first match.
        if let Some(t) = self.tee.as_mut() {
            t.write_all(chunk)?;
        }
        if self.halted || self.deferred_err.is_some() {
            // Consume silently to let decompress finish cleanly.
            return Ok(chunk.len());
        }
        let mut start = 0usize;
        for i in 0..chunk.len() {
            if chunk[i] == b'\n' {
                let slice = &chunk[start..i];
                if let Err(e) = self.scan_line(slice) {
                    self.deferred_err = Some(e);
                    return Ok(chunk.len());
                }
                start = i + 1;
                if self.halted {
                    break;
                }
            }
        }
        // Keep the trailing partial line (if any) for the next chunk.
        if start < chunk.len() && !self.halted {
            self.buf.extend_from_slice(&chunk[start..]);
        }
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(t) = self.tee.as_mut() {
            t.flush()?;
        }
        Ok(())
    }
}

fn emit_match(
    out: &Arc<Mutex<Box<dyn Write + Send>>>,
    opts: &GrepOptions,
    archive: &str,
    entry: &str,
    line_no: u64,
    line: &[u8],
) -> Result<()> {
    let mut out = out.lock().unwrap();
    if opts.files_only {
        writeln!(out, "{}:{}", archive, entry)?;
        return Ok(());
    }
    if opts.json {
        // Lossy UTF-8 conversion for JSON — binary matches aren't
        // JSON-representable, so losing invalid sequences is honest.
        let line_str = String::from_utf8_lossy(line);
        let rec = MatchRecord {
            archive,
            entry,
            line: line_no,
            r#match: &line_str,
        };
        let ser = serde_json::to_string(&rec).context("serialize match JSON")?;
        writeln!(out, "{}", ser)?;
    } else {
        // Grep-style. Escape non-printable bytes crudely to avoid
        // terminal glyph confusion from arbitrary binary data.
        let line_str = String::from_utf8_lossy(line);
        writeln!(out, "{}:{}:{}:{}", archive, entry, line_no, line_str)?;
    }
    Ok(())
}

// ── CDR filters ────────────────────────────────────────────────────────────

/// `true` when `pattern` is a plain literal — no regex metacharacters.
/// Used to decide whether the `.tzip/terms.v1` index can safely narrow
/// the candidate set. Complex patterns always run through linear scan.
///
/// We deliberately miss some patterns that *could* benefit from the
/// index (e.g. `"error.*memory"` has two literal substrings) rather
/// than introduce a regex→literal-extractor dependency; the common
/// case is plain-literal user queries.
fn is_plain_literal(pattern: &str) -> bool {
    // Any of these characters means regex syntax; bail out.
    !pattern
        .chars()
        .any(|c| matches!(c, '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'))
}

fn entry_passes_filters(re: &ReadEntry, opts: &GrepOptions) -> bool {
    // Skip tzip's own index sentinel entries — they're machine-
    // generated metadata (name MPH, term postings, facet counters,
    // Parquet sidecar), not user data. Grep matching inside
    // `.tzip/terms.v1` would surface every indexed token as a
    // false hit. Users who legitimately want to grep inside them
    // can pass an explicit `--include '.tzip/*'`.
    if re.name.starts_with(".tzip/")
        && !opts.include.iter().any(|g| g.starts_with(".tzip/"))
    {
        return false;
    }
    if !opts.include.is_empty() && !opts.include.iter().any(|g| glob_match(g, &re.name)) {
        return false;
    }
    if opts.exclude.iter().any(|g| glob_match(g, &re.name)) {
        return false;
    }
    if let Some(min) = opts.min_size {
        if re.uncomp_size < min {
            return false;
        }
    }
    if let Some(max) = opts.max_size {
        if re.uncomp_size > max {
            return false;
        }
    }
    if opts.since.is_some() || opts.until.is_some() {
        let ts = dos_to_unix(re.mtime);
        if let Some(s) = opts.since {
            if ts < s {
                return false;
            }
        }
        if let Some(u) = opts.until {
            if ts > u {
                return false;
            }
        }
    }
    true
}

/// Simple `*`/`?` glob. Mirrors `walker::glob_match` + the matcher in
/// `sources::s3_list` for consistency across tzip.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn recur(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if recur(&p[1..], t) {
                    return true;
                }
                if let Some((_, rest)) = t.split_first() {
                    return recur(p, rest);
                }
                false
            }
            (Some(b'?'), Some(_)) => recur(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => recur(&p[1..], &t[1..]),
            _ => false,
        }
    }
    recur(pattern.as_bytes(), text.as_bytes())
}

use crate::dos_time::dos_to_unix;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Regex builder ──────────────────────────────────────────────────────────

fn build_regex(pattern: &str, ignore_case: bool) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .with_context(|| format!("compile regex /{}/", pattern))
}

// ── Checkpoint ─────────────────────────────────────────────────────────────

struct CheckpointState {
    skip: HashSet<(String, String)>,
    writer: Arc<Mutex<File>>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct CheckpointHeader {
    v: u32,
    pattern: String,
    ignore_case: bool,
    archives: Vec<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    since: Option<i64>,
    until: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckpointEntry {
    archive: String,
    entry: String,
    matches: u64,
    ts: i64,
}

#[derive(Debug, Serialize)]
struct MatchRecord<'a> {
    archive: &'a str,
    entry: &'a str,
    line: u64,
    // `match` is a Rust keyword; serde emits the raw name.
    #[serde(rename = "match")]
    r#match: &'a str,
}

fn checkpoint_init(opts: &GrepOptions, archives: &[String]) -> Result<Option<CheckpointState>> {
    let path = match &opts.checkpoint {
        Some(p) => p.clone(),
        None => return Ok(None),
    };
    let header = CheckpointHeader {
        v: CHECKPOINT_VERSION,
        pattern: opts.pattern.clone(),
        ignore_case: opts.ignore_case,
        archives: archives.to_vec(),
        include: opts.include.clone(),
        exclude: opts.exclude.clone(),
        min_size: opts.min_size,
        max_size: opts.max_size,
        since: opts.since,
        until: opts.until,
    };

    if opts.resume {
        if !path.exists() {
            bail!("--resume: checkpoint file {} does not exist", path.display());
        }
        let existing = fs_read_checkpoint(&path)?;
        if existing.header != header {
            bail!(
                "--resume: checkpoint at {} was written for a different query \
                 (pattern / archive set / filters changed); refusing to resume",
                path.display()
            );
        }
        let f = OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("open checkpoint {} for append", path.display()))?;
        return Ok(Some(CheckpointState {
            skip: existing.completed,
            writer: Arc::new(Mutex::new(f)),
        }));
    }

    // Fresh checkpoint: truncate + write header line.
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("create checkpoint {}", path.display()))?;
    let header_line = serde_json::to_string(&header)?;
    writeln!(f, "{}", header_line)?;
    f.sync_data().ok();
    Ok(Some(CheckpointState {
        skip: HashSet::new(),
        writer: Arc::new(Mutex::new(f)),
    }))
}

struct ParsedCheckpoint {
    header: CheckpointHeader,
    completed: HashSet<(String, String)>,
}

fn fs_read_checkpoint(path: &Path) -> Result<ParsedCheckpoint> {
    let f = File::open(path)
        .with_context(|| format!("open checkpoint {}", path.display()))?;
    let mut lines = BufReader::new(f).lines();
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow!("empty checkpoint {}", path.display()))??;
    let header: CheckpointHeader = serde_json::from_str(&header_line)
        .with_context(|| format!("parse checkpoint header in {}", path.display()))?;
    let mut completed: HashSet<(String, String)> = HashSet::new();
    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: CheckpointEntry = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue, // tolerate a half-written trailing line
        };
        completed.insert((rec.archive, rec.entry));
    }
    Ok(ParsedCheckpoint { header, completed })
}

// ── guard so the Sender isn't flagged unused when inner-jobs is 1 ──────────
#[allow(dead_code)]
fn _sender_lives(_: Sender<ReadEntry>) {}
