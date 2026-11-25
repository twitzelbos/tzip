//! `tzip diff a.zip b.zip` — body-free CDR comparison. Opens both
//! archives, reads only metadata (name / size / crc32 / mtime), and
//! reports which entries were added in `b`, removed from `a`, or
//! changed. Never fetches an entry body.
//!
//! Classification rules:
//!   - **added**:  entry name in `b` only
//!   - **removed**: entry name in `a` only
//!   - **changed**: name in both, but size or crc32 differ (strongest
//!     content signal available without decompression)
//!   - **mtime-only**: name in both, size + crc32 match but mtime
//!     differs. Suppressed by default — "repackage same contents"
//!     commonly shifts mtime without changing content. Surface them
//!     with `--mtime`.
//!
//! Exit status follows the `diff` convention: 0 = identical, 1 =
//! differed, 2 = error. Suitable for `if tzip diff a b; then ...`
//! shell pipelines.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;

use crate::cli::DiffOptions;
use crate::reader::open_archive;
use crate::zipreader::ReadEntry;

/// Returns the shell exit status: 0 = identical, 1 = differed.
/// Errors propagate through `Result`; `main` maps them to exit 2.
pub fn run(opts: DiffOptions) -> Result<i32> {
    let arch_a = open_archive(&opts.a)
        .with_context(|| format!("open archive a: {}", opts.a))?;
    let arch_b = open_archive(&opts.b)
        .with_context(|| format!("open archive b: {}", opts.b))?;

    let mut map_a: HashMap<String, EntryMeta> = HashMap::new();
    for e in arch_a.iter_entries() {
        if !passes_filters(&e.name, &opts.include, &opts.exclude) {
            continue;
        }
        map_a.insert(e.name.clone(), meta_from(e));
    }
    let mut map_b: HashMap<String, EntryMeta> = HashMap::new();
    for e in arch_b.iter_entries() {
        if !passes_filters(&e.name, &opts.include, &opts.exclude) {
            continue;
        }
        map_b.insert(e.name.clone(), meta_from(e));
    }

    // Iterate the union, sorted so output is deterministic across runs.
    let names: BTreeSet<&String> = map_a.keys().chain(map_b.keys()).collect();
    let mut records: Vec<DiffRecord> = Vec::new();
    for name in names {
        let a = map_a.get(name);
        let b = map_b.get(name);
        let record = match (a, b) {
            (Some(a), None) => DiffRecord::removed(name, a),
            (None, Some(b)) => DiffRecord::added(name, b),
            (Some(a), Some(b)) => {
                let content_changed = a.size != b.size || a.crc32 != b.crc32;
                if content_changed {
                    DiffRecord::changed(name, a, b)
                } else if opts.mtime && a.mtime != b.mtime {
                    DiffRecord::mtime_only(name, a, b)
                } else {
                    continue;
                }
            }
            (None, None) => continue,
        };
        records.push(record);
    }

    let differed = !records.is_empty();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if opts.json {
        for r in &records {
            writeln!(out, "{}", serde_json::to_string(r).unwrap())?;
        }
    } else if opts.names_only {
        for r in &records {
            writeln!(out, "{} {}", status_char(&r.status), r.entry)?;
        }
    } else {
        emit_table(&mut out, &records)?;
    }

    if !opts.quiet && opts.verbose {
        eprintln!(
            "tzip diff: {} differences (added {}, removed {}, changed {}, mtime-only {})",
            records.len(),
            records.iter().filter(|r| r.status == Status::Added).count(),
            records.iter().filter(|r| r.status == Status::Removed).count(),
            records.iter().filter(|r| r.status == Status::Changed).count(),
            records.iter().filter(|r| r.status == Status::MtimeOnly).count(),
        );
    }

    Ok(if differed { 1 } else { 0 })
}

// ── Records ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Added,
    Removed,
    Changed,
    MtimeOnly,
}

fn status_char(s: &Status) -> char {
    match s {
        Status::Added => '+',
        Status::Removed => '-',
        Status::Changed => '~',
        Status::MtimeOnly => 'T',
    }
}

#[derive(Debug, Clone, Serialize)]
struct DiffRecord {
    status: Status,
    entry: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_a: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_b: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    crc32_a: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    crc32_b: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime_a: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime_b: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct EntryMeta {
    size: u64,
    crc32: u32,
    mtime: (u16, u16),
}

fn meta_from(e: &ReadEntry) -> EntryMeta {
    EntryMeta {
        size: e.uncomp_size,
        crc32: e.crc32,
        mtime: e.mtime,
    }
}

impl DiffRecord {
    fn added(name: &str, b: &EntryMeta) -> Self {
        Self {
            status: Status::Added,
            entry: name.to_string(),
            size_a: None,
            size_b: Some(b.size),
            crc32_a: None,
            crc32_b: Some(b.crc32),
            mtime_a: None,
            mtime_b: Some(dos_to_unix(b.mtime)),
        }
    }
    fn removed(name: &str, a: &EntryMeta) -> Self {
        Self {
            status: Status::Removed,
            entry: name.to_string(),
            size_a: Some(a.size),
            size_b: None,
            crc32_a: Some(a.crc32),
            crc32_b: None,
            mtime_a: Some(dos_to_unix(a.mtime)),
            mtime_b: None,
        }
    }
    fn changed(name: &str, a: &EntryMeta, b: &EntryMeta) -> Self {
        Self {
            status: Status::Changed,
            entry: name.to_string(),
            size_a: Some(a.size),
            size_b: Some(b.size),
            crc32_a: Some(a.crc32),
            crc32_b: Some(b.crc32),
            mtime_a: Some(dos_to_unix(a.mtime)),
            mtime_b: Some(dos_to_unix(b.mtime)),
        }
    }
    fn mtime_only(name: &str, a: &EntryMeta, b: &EntryMeta) -> Self {
        Self {
            status: Status::MtimeOnly,
            entry: name.to_string(),
            size_a: Some(a.size),
            size_b: Some(b.size),
            crc32_a: Some(a.crc32),
            crc32_b: Some(b.crc32),
            mtime_a: Some(dos_to_unix(a.mtime)),
            mtime_b: Some(dos_to_unix(b.mtime)),
        }
    }
}

// ── Table output ──────────────────────────────────────────────────────────

fn emit_table<W: Write>(w: &mut W, records: &[DiffRecord]) -> std::io::Result<()> {
    for r in records {
        match r.status {
            Status::Added => {
                writeln!(
                    w,
                    "+ {:<8} {}  ({} bytes, crc {:08x})",
                    "added",
                    r.entry,
                    r.size_b.unwrap_or(0),
                    r.crc32_b.unwrap_or(0),
                )?;
            }
            Status::Removed => {
                writeln!(
                    w,
                    "- {:<8} {}  ({} bytes, crc {:08x})",
                    "removed",
                    r.entry,
                    r.size_a.unwrap_or(0),
                    r.crc32_a.unwrap_or(0),
                )?;
            }
            Status::Changed => {
                writeln!(
                    w,
                    "~ {:<8} {}  ({} \u{2192} {} bytes, crc {:08x} \u{2192} {:08x})",
                    "changed",
                    r.entry,
                    r.size_a.unwrap_or(0),
                    r.size_b.unwrap_or(0),
                    r.crc32_a.unwrap_or(0),
                    r.crc32_b.unwrap_or(0),
                )?;
            }
            Status::MtimeOnly => {
                writeln!(
                    w,
                    "T {:<8} {}  (mtime {} \u{2192} {})",
                    "mtime",
                    r.entry,
                    r.mtime_a.unwrap_or(0),
                    r.mtime_b.unwrap_or(0),
                )?;
            }
        }
    }
    Ok(())
}

// ── Filters ───────────────────────────────────────────────────────────────

fn passes_filters(name: &str, include: &[String], exclude: &[String]) -> bool {
    if !include.is_empty() && !include.iter().any(|g| glob_match(g, name)) {
        return false;
    }
    if exclude.iter().any(|g| glob_match(g, name)) {
        return false;
    }
    true
}

/// `*` / `?` glob. Same shape as find.rs / grep.rs for consistency.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches() {
        assert!(glob_match("*.log", "foo.log"));
        assert!(!glob_match("*.log", "foo.txt"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn status_char_table() {
        assert_eq!(status_char(&Status::Added), '+');
        assert_eq!(status_char(&Status::Removed), '-');
        assert_eq!(status_char(&Status::Changed), '~');
        assert_eq!(status_char(&Status::MtimeOnly), 'T');
    }

    #[test]
    fn dos_to_unix_epoch() {
        // 1980-01-01 00:00:00 = (date=0x0021, time=0)
        let ts = dos_to_unix((0x0021, 0));
        // 1980-01-01 00:00 UTC = 315532800
        assert_eq!(ts, 315_532_800);
    }
}
