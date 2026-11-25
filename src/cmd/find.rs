//! `tzip find <archive>...` — body-free metadata query across many
//! archives. Lists archive entries that match CDR-side filters (glob
//! include/exclude, size range, mtime range) without ever fetching an
//! entry body, so it's essentially free on remote archives: one HEAD +
//! one CDR fetch per archive, then pure in-memory filtering.
//!
//! Shares the archive-set expansion and `-P` outer fan-out with
//! `tzip grep`; skips the per-archive inner workers entirely because
//! there is no per-entry I/O.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::cli::FindOptions;
use crate::reader::open_archive;
use crate::sources::s3_list;
use crate::zipreader::ReadEntry;

pub fn run(opts: FindOptions) -> Result<()> {
    let archives = expand_archives(&opts.archives)?;
    if archives.is_empty() {
        if !opts.quiet {
            eprintln!("tzip: no archives matched the given patterns");
        }
        return Ok(());
    }
    if opts.verbose {
        eprintln!("tzip: finding across {} archive(s)", archives.len());
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.parallel)
        .thread_name(|i| format!("tzip-find-outer-{}", i))
        .build()
        .context("build find rayon pool")?;
    let out: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let opts = Arc::new(opts);

    pool.install(|| -> Result<()> {
        use rayon::prelude::*;
        archives.par_iter().try_for_each(|url| -> Result<()> {
            process_archive(url, Arc::clone(&opts), Arc::clone(&out))
        })
    })?;
    Ok(())
}

fn process_archive(
    url: &str,
    opts: Arc<FindOptions>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
) -> Result<()> {
    let driver = open_archive(url)
        .with_context(|| format!("open archive {}", url))?;
    // Buffer this archive's hits so a single lock-and-write emits them
    // contiguously — avoids interleaving many archives mid-line.
    let mut buf: Vec<u8> = Vec::new();
    for re in driver.iter_entries() {
        if !entry_passes_filters(re, &opts) {
            continue;
        }
        emit(&mut buf, url, re, &opts)?;
    }
    if !buf.is_empty() {
        let mut out = out.lock().unwrap();
        out.write_all(&buf)?;
    }
    Ok(())
}

fn emit(buf: &mut Vec<u8>, url: &str, re: &ReadEntry, opts: &FindOptions) -> Result<()> {
    if opts.names_only {
        writeln!(buf, "{}:{}", url, re.name)?;
        return Ok(());
    }
    if opts.json {
        let rec = EntryRecord {
            archive: url,
            entry: &re.name,
            size: re.uncomp_size,
            compressed_size: re.comp_size,
            method: re.inner_method,
            encrypted: re.encrypted,
            mtime: dos_to_unix(re.mtime),
            crc32: re.crc32,
            comment: &re.comment,
            unix_mode: re.unix_mode().map(|m| m & 0o7777),
        };
        let line = serde_json::to_string(&rec).context("serialize find JSON")?;
        writeln!(buf, "{}", line)?;
        return Ok(());
    }
    // Default tabular: archive | mtime (ISO) | size | entry.
    writeln!(
        buf,
        "{}\t{}\t{:>12}\t{}",
        url,
        iso_date(dos_to_unix(re.mtime)),
        re.uncomp_size,
        re.name,
    )?;
    Ok(())
}

// ── CDR filters (shared shape with grep) ──────────────────────────────────

fn entry_passes_filters(re: &ReadEntry, opts: &FindOptions) -> bool {
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

/// Format a Unix timestamp as `YYYY-MM-DD HH:MM:SS` (UTC). Inverse of
/// `civil_to_unix` + time-of-day; avoids pulling in `chrono` for one
/// human-readable column.
fn iso_date(ts: i64) -> String {
    // Days since epoch, floor-div so negative timestamps still work.
    let days = ts.div_euclid(86_400);
    let tod = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = (tod / 3600) as u32;
    let mm = ((tod % 3600) / 60) as u32;
    let ss = (tod % 60) as u32;
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
}

/// Inverse of Howard Hinnant's `days_from_civil` — days since 1970-01-01
/// back to (y, m, d).
use crate::dos_time::civil_from_days;

// ── Archive expansion ─────────────────────────────────────────────────────

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
        out.push(spec.clone());
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
struct EntryRecord<'a> {
    archive: &'a str,
    entry: &'a str,
    size: u64,
    compressed_size: u64,
    method: u16,
    encrypted: bool,
    mtime: i64,
    crc32: u32,
    /// Per-entry CDR comment. Empty string if the entry has no comment.
    #[serde(skip_serializing_if = "str::is_empty")]
    comment: &'a str,
    /// Unix mode bits (file-permission portion) when the archive was
    /// produced on a Unix host. Null on DOS/Windows-origin archives.
    #[serde(skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_date_known_epoch() {
        assert_eq!(iso_date(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn iso_date_known_2000() {
        // 2000-01-01 00:00:00 UTC = 946684800
        assert_eq!(iso_date(946_684_800), "2000-01-01 00:00:00");
    }

    #[test]
    fn iso_date_roundtrips_civil_to_unix() {
        let ts = crate::dos_time::civil_to_unix(2026, 11, 23);
        assert_eq!(iso_date(ts), "2026-11-23 00:00:00");
    }

    #[test]
    fn glob_matches_tail() {
        assert!(glob_match("*.log", "corpus/a.log"));
        assert!(!glob_match("*.log", "corpus/a.txt"));
    }
}
