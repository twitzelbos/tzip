//! `tzip tail [-n N] [--by {added,mtime}] <archive>...` — the last
//! N entries of one or more archives, sorted either by the order
//! they were added or by their mtime. Issue #71.
//!
//! Like `tzip find`, this command is **body-free** — it reads only
//! the Central Directory of each archive, so it's essentially free
//! on remote archives (one HEAD + one CDR fetch per archive). No
//! bodies, no decrypt, no decompress.
//!
//! ## What "added" and "mtime" mean here (issue #77)
//!
//! A ZIP entry carries one timestamp: the **mtime of the source
//! file at the moment the archive was written**. It's stored as a
//! DOS-format 2-second-resolution timestamp in both the Local File
//! Header and the Central Directory entry (plus an optional Info-
//! ZIP Unix extra field for ns precision, which tzip reads if
//! present).
//!
//! ZIP does **not** record an "added to archive" timestamp —
//! there's no field for it. `--by added` therefore uses the entry's
//! position in the Central Directory as a proxy:
//!
//! - tzip's append mode (`tzip u` on an existing archive) keeps
//!   all old entries at their original CDR positions and lays new
//!   entries *after* them. So "last in CDR order" ≈ "most
//!   recently added" on an append-only history.
//!
//! - tzip's rebuild mode (triggered by `--delete`, `--sort`,
//!   `--atomic`, or a name collision) rewrites the whole CDR in
//!   its own order, which breaks the "CDR position = added-at
//!   order" proxy for that one write. See the "Known limits"
//!   section below.
//!
//! `--by mtime` sorts by the DOS mtime regardless of CDR order —
//! it gives you the archive's view of the *source file's* latest
//! modification time, not the archive-write time.
//!
//! ## Known limits
//!
//! - The "added at" proxy via CDR position is approximate once an
//!   archive has been rebuilt (see above). A dedicated tzip-native
//!   extra field recording the archive-write timestamp per entry
//!   is the proper fix and is left as a follow-up; the current
//!   proxy is good enough for the common case (append-only
//!   workloads).
//! - Multi-shard fan-out: results are merged across all passed
//!   archives, then the global top-N is emitted. One tzip process
//!   per shard would get you per-shard top-N; here we do the
//!   merged form because that's what most "latest N across my
//!   archive set" questions actually want.

use anyhow::{Context, Result};
use std::io::Write;

use crate::cli::{TailBy, TailOptions};
use crate::dos_time::{civil_from_days, dos_to_unix};
use crate::reader::open_archive;
use crate::sources::s3_list;
use crate::zipreader::ReadEntry;

pub fn run(opts: TailOptions) -> Result<()> {
    let archives = expand_archives(&opts.archives)?;
    if archives.is_empty() {
        if !opts.quiet {
            eprintln!("tzip: no archives matched the given patterns");
        }
        return Ok(());
    }

    // Collect `(sort_key, archive_url, entry_name, size, mtime,
    // encrypted, method)` for every surviving entry across every
    // shard, then partial-sort + truncate to N. Even at a million
    // entries this is only a few tens of MB of in-memory tuples,
    // and `tail` is a fast-interactive command, not a daily
    // batch job. If that ever bites: a bounded min-heap of size N
    // per archive + global merge would collapse the peak.
    let mut rows: Vec<Row> = Vec::new();
    for url in &archives {
        let driver = open_archive(url)
            .with_context(|| format!("open archive {}", url))?;
        for re in driver.iter_entries() {
            if skip_tzip_sentinel(&re.name) {
                continue;
            }
            rows.push(Row::from_entry(url, re, re_position(&rows, url)));
        }
    }

    sort_rows(&mut rows, opts.by);
    rows.truncate(opts.n);

    let mut out = std::io::stdout().lock();
    for r in &rows {
        if opts.json {
            writeln!(
                out,
                "{{\"archive\":{},\"entry\":{},\"size\":{},\"mtime\":{},\"encrypted\":{},\"method\":{}}}",
                json_str(&r.archive),
                json_str(&r.name),
                r.size,
                r.mtime,
                r.encrypted,
                r.method,
            )?;
        } else {
            writeln!(
                out,
                "{}\t{}\t{:>12}\t{}",
                r.archive,
                iso_date(r.mtime),
                r.size,
                r.name,
            )?;
        }
    }
    Ok(())
}

/// Collected per-entry tuple. `cdr_position` is this entry's
/// index inside its *own* archive's CDR — used by `--by added`
/// to approximate "added at" order; see the module comment.
struct Row {
    archive: String,
    name: String,
    size: u64,
    mtime: i64,
    encrypted: bool,
    method: u16,
    cdr_position: usize,
}

impl Row {
    fn from_entry(url: &str, re: &ReadEntry, cdr_position: usize) -> Self {
        Self {
            archive: url.to_string(),
            name: re.name.clone(),
            size: re.uncomp_size,
            mtime: dos_to_unix(re.mtime),
            encrypted: re.encrypted,
            method: re.inner_method,
            cdr_position,
        }
    }
}

/// `rows` is populated in input-archive order, so the number of
/// rows whose `archive` equals `url` so far is this new row's
/// CDR position within its archive.
fn re_position(rows: &[Row], url: &str) -> usize {
    rows.iter().rev().take_while(|r| r.archive == url).count()
}

fn sort_rows(rows: &mut [Row], by: TailBy) {
    match by {
        TailBy::Added => {
            // "Latest added" across the whole set:
            // rank primarily by CDR position (higher = newer),
            // break ties by archive URL so the order is stable.
            // We sort DESCENDING on the composite key so the
            // `truncate(opts.n)` keeps the top N.
            rows.sort_by(|a, b| {
                b.cdr_position
                    .cmp(&a.cdr_position)
                    .then_with(|| a.archive.cmp(&b.archive))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
        TailBy::Mtime => {
            rows.sort_by(|a, b| {
                b.mtime
                    .cmp(&a.mtime)
                    .then_with(|| a.archive.cmp(&b.archive))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
    }
}

/// Skip the `.tzip/*` sentinel entries — they're tzip's own
/// bookkeeping, not user data, so a `tzip tail` run shouldn't
/// surface e.g. `.tzip/names.v1` as "most recently added."
fn skip_tzip_sentinel(name: &str) -> bool {
    name.starts_with(".tzip/")
}

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

fn iso_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let tod = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = (tod / 3600) as u32;
    let mm = ((tod % 3600) / 60) as u32;
    let ss = (tod % 60) as u32;
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
}

/// Minimal JSON string escaping — covers the chars tzip entry
/// names and archive URLs actually use. Not a full RFC 8259
/// escaper, but these fields never contain NUL or raw control
/// bytes in practice.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(url: &str, name: &str, mtime: i64, pos: usize) -> Row {
        Row {
            archive: url.into(),
            name: name.into(),
            size: 0,
            mtime,
            encrypted: false,
            method: 0,
            cdr_position: pos,
        }
    }

    #[test]
    fn sort_added_puts_highest_cdr_first() {
        let mut rows = vec![
            r("a.zip", "x", 100, 0),
            r("a.zip", "y", 100, 5),
            r("a.zip", "z", 100, 2),
        ];
        sort_rows(&mut rows, TailBy::Added);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["y", "z", "x"]);
    }

    #[test]
    fn sort_mtime_puts_newest_first() {
        let mut rows = vec![
            r("a.zip", "old", 100, 0),
            r("a.zip", "mid", 200, 0),
            r("a.zip", "new", 300, 0),
        ];
        sort_rows(&mut rows, TailBy::Mtime);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["new", "mid", "old"]);
    }

    #[test]
    fn mtime_tie_breaks_by_archive_then_name() {
        let mut rows = vec![
            r("b.zip", "a", 100, 0),
            r("a.zip", "b", 100, 0),
            r("a.zip", "a", 100, 0),
        ];
        sort_rows(&mut rows, TailBy::Mtime);
        let out: Vec<_> = rows.iter().map(|r| (r.archive.as_str(), r.name.as_str())).collect();
        assert_eq!(out, vec![("a.zip", "a"), ("a.zip", "b"), ("b.zip", "a")]);
    }

    #[test]
    fn sentinel_entries_are_skipped() {
        assert!(skip_tzip_sentinel(".tzip/names.v1"));
        assert!(skip_tzip_sentinel(".tzip/columnar.v1.parquet"));
        assert!(!skip_tzip_sentinel("logs/a.json"));
    }

    #[test]
    fn re_position_counts_tail_from_same_archive_only() {
        let rows = vec![
            r("a.zip", "x", 0, 0),
            r("a.zip", "y", 0, 1),
            r("b.zip", "x", 0, 0),
        ];
        // Next row for a.zip would land at position 2; for a
        // fresh archive it starts at 0.
        assert_eq!(re_position(&rows, "a.zip"), 0);
        // That's because the last row was b.zip — the counter
        // only scans the trailing run of matching URLs. Which
        // is fine: Row::from_entry is called while iterating a
        // single archive's entries, so the tail-run matches.
        assert_eq!(re_position(&rows, "b.zip"), 1);
    }

    #[test]
    fn json_str_escapes_quotes_and_slashes() {
        assert_eq!(json_str("plain"), "\"plain\"");
        assert_eq!(json_str("has \"quote\""), "\"has \\\"quote\\\"\"");
        assert_eq!(json_str("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(json_str("tab\there"), "\"tab\\there\"");
    }
}
