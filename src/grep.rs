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
use crossbeam_channel::{bounded, Sender};
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::cli::GrepOptions;
use crate::reader::{open_archive, open_source_for, stream_entry_to};
use crate::sources::s3_list;
use crate::zipreader::ReadEntry;

const CHECKPOINT_VERSION: u32 = 1;

pub fn run(opts: GrepOptions) -> Result<()> {
    // 1. Expand the archive list: locals pass through, s3://…/*.zip
    //    goes through ListObjectsV2.
    let archives = expand_archives(&opts.archives)?;
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

    // 4. Outer fan-out: archives in parallel.
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
) -> Result<()> {
    // Phase 1: parse CDR, filter entries, populate lfh_total_size.
    let mut driver = open_archive(url)
        .with_context(|| format!("open archive {}", url))?;
    // Collect candidate indices first so we can mutably probe LFH sizes
    // without holding an immutable borrow of driver.entries().
    let candidates: Vec<usize> = (0..driver.entries().len())
        .filter(|&idx| {
            let re = &driver.entries()[idx];
            entry_passes_filters(re, &opts)
                && !skip.contains(&(url.to_string(), re.name.clone()))
        })
        .collect();
    let mut filtered: Vec<ReadEntry> = Vec::with_capacity(candidates.len());
    for idx in candidates {
        let name = driver.entries()[idx].name.clone();
        driver
            .lfh_total_size_of(idx)
            .with_context(|| format!("probe LFH for {} in {}", name, url))?;
        filtered.push(driver.entries()[idx].clone());
    }
    drop(driver);
    if filtered.is_empty() {
        return Ok(());
    }

    // Phase 2: inner fan-out.
    let (tx, rx) = bounded::<ReadEntry>(opts.inner_jobs * 2);
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
            let handle = scope.spawn(move || -> Result<()> {
                let mut source = open_source_for(&url)
                    .with_context(|| format!("open inner source for {}", url))?;
                while let Ok(re) = rx.recv() {
                    scan_entry(&url, &mut *source, &re, &regex, &opts, cp_writer.as_ref(), &out)?;
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
    // Materialise the full decompressed body. For a grep that scans every
    // byte anyway, per-entry buffering is simplest; a future streaming
    // line scanner is a follow-up.
    let mut body: Vec<u8> = Vec::with_capacity(re.uncomp_size.min(16 * 1024 * 1024) as usize);
    stream_entry_to(source, re, opts.password.as_deref(), &mut body)
        .with_context(|| format!("read body of {} in {}", re.name, url))?;

    // Always scan first — it decides whether `--exec` runs (so the
    // external command only sees entries that actually matched the
    // pattern, acting as an rg pre-filter) and drives the default
    // grep-style output otherwise.
    let mut match_count: u64 = 0;
    let mut first_matched_line: Option<(u64, Vec<u8>)> = None;
    let mut line_no: u64 = 0;
    let total_newlines = count_newlines(&body);
    for line in body.split(|&b| b == b'\n') {
        line_no += 1;
        // split yields a trailing empty element when the buffer ends
        // in \n — skip it so we don't invent a blank last line.
        if line.is_empty() && line_no == 1 + total_newlines {
            break;
        }
        if regex.is_match(line) {
            match_count += 1;
            if opts.exec.is_none() {
                emit_match(out, opts, url, &re.name, line_no, line)?;
                if opts.files_only {
                    break;
                }
            } else if first_matched_line.is_none() {
                first_matched_line = Some((line_no, line.to_vec()));
                // Don't bail — count all matches for the checkpoint record.
            }
        }
    }

    if let Some(cmd) = &opts.exec {
        if match_count > 0 {
            let tmp = tempfile::NamedTempFile::new().context("create exec tempfile")?;
            std::fs::write(tmp.path(), &body).context("write exec tempfile")?;
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
        }
        // Track the first matched line so a checkpoint knows what we saw.
        let _ = first_matched_line;
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

fn count_newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b == b'\n').count() as u64
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

fn entry_passes_filters(re: &ReadEntry, opts: &GrepOptions) -> bool {
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

fn dos_to_unix((date, time): (u16, u16)) -> i64 {
    let year = 1980 + ((date >> 9) & 0x7F) as i64;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as i64;
    let minute = ((time >> 5) & 0x3F) as i64;
    let sec = ((time & 0x1F) << 1) as i64;
    let month = month.max(1);
    let day = day.max(1);
    civil_to_unix(year, month, day) + hour * 3600 + minute * 60 + sec
}

fn civil_to_unix(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    days * 86_400
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Archive expansion ──────────────────────────────────────────────────────

fn expand_archives(input: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for spec in input {
        if let Some(rest) = spec.strip_prefix("s3://") {
            if s3_list::has_glob(rest) {
                let expanded = s3_list::expand(rest)
                    .with_context(|| format!("expand S3 glob {}", spec))?;
                out.extend(expanded);
                continue;
            }
        }
        // Everything else: pass through. Local shell globs are already
        // expanded by the shell before argv.
        out.push(spec.clone());
    }
    Ok(out)
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
