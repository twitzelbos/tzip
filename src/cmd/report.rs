//! Shared "incomplete-result" reporting for the read-side
//! subcommands (`grep`, `hash`, `jq`, `aggregate`, `find`).
//!
//! The read commands all have places where they tolerate a per-
//! entry failure rather than killing the whole run: a single
//! corrupt entry, a bad password on one entry out of thousands, a
//! transient network glitch after the retry budget is exhausted,
//! a 412 Precondition Failed from the new snapshot-consistency
//! path, a facet segment whose body won't decompress, etc.
//! Before this module, those failures emitted an `eprintln!`
//! and otherwise vanished — the process exited 0, downstream
//! pipelines treated the output as complete, and the user had
//! no way to assert "I need an authoritative answer; fail if
//! anything is missing."
//!
//! `RunReport` fixes that. Each subcommand maintains one
//! (behind an `Arc<Mutex<_>>`), records every skip with a short
//! reason, and on completion:
//!
//! - prints a one-line summary to stderr when anything was
//!   skipped, so the operator sees it even in plain output;
//! - emits a trailing `{"report": {...}}` line in `--json` mode
//!   with full skip details, so downstream machinery sees it;
//! - returns a non-zero exit when `--strict` was passed and any
//!   skip occurred — the explicit opt-in for pipelines that
//!   want no-tolerance semantics.

use std::sync::{Arc, Mutex};

/// One recorded skip event. `where_` is the archive (and
/// optionally the entry name), `reason` is a short human string.
#[derive(Clone, Debug)]
pub struct SkipEvent {
    pub where_: String,
    pub reason: String,
}

/// Aggregate report of a read-side run. Cloneable via `Arc` so
/// worker threads can hand it around; internal state lives
/// behind a `Mutex` because workers are parallel.
#[derive(Clone, Default)]
pub struct RunReport {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    ok: usize,
    skipped: Vec<SkipEvent>,
}

impl RunReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the OK counter. Call once per entry that
    /// completed its scan / hash / filter cleanly.
    pub fn incr_ok(&self) {
        let mut g = self.inner.lock().expect("report mutex");
        g.ok += 1;
    }

    /// Record a skipped entry. `where_` should be enough for
    /// an operator to locate the entry (e.g. `archive:entry`);
    /// `reason` is a one-line summary of what went wrong.
    pub fn record_skip(&self, where_: impl Into<String>, reason: impl Into<String>) {
        let mut g = self.inner.lock().expect("report mutex");
        g.skipped.push(SkipEvent {
            where_: where_.into(),
            reason: reason.into(),
        });
    }

    pub fn skipped_count(&self) -> usize {
        self.inner.lock().expect("report mutex").skipped.len()
    }

    pub fn ok_count(&self) -> usize {
        self.inner.lock().expect("report mutex").ok
    }

    pub fn snapshot(&self) -> (usize, Vec<SkipEvent>) {
        let g = self.inner.lock().expect("report mutex");
        (g.ok, g.skipped.clone())
    }
}

/// Emit the end-of-run summary. If anything was skipped:
///
/// - In **plain mode** (`json = false`), prints the summary to
///   stderr: `tzip <cmd>: incomplete — N of M entries skipped
///   (first: <reason>)`. One line, trivially greppable.
/// - In **JSON mode** (`json = true`), prints a final line to
///   stdout: `{"report": {"complete": false, "ok": N,
///   "skipped": [...]}}`. Parsers already consuming JSONL see
///   it as "one more object."
///
/// Returns `true` if the caller should exit with a non-zero
/// status: that happens iff `strict && skipped > 0`.
pub fn finalize(report: &RunReport, cmd: &str, strict: bool, json: bool) -> bool {
    let (ok, skipped) = report.snapshot();
    if skipped.is_empty() {
        return false;
    }
    let total = ok + skipped.len();
    if json {
        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let mut buf = String::new();
        buf.push_str(r#"{"report":{"complete":false,"ok":"#);
        buf.push_str(&ok.to_string());
        buf.push_str(r#","total":"#);
        buf.push_str(&total.to_string());
        buf.push_str(r#","skipped":["#);
        for (i, ev) in skipped.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(r#"{"where":"#);
            buf.push_str(&json_string(&ev.where_));
            buf.push_str(r#","reason":"#);
            buf.push_str(&json_string(&ev.reason));
            buf.push('}');
        }
        buf.push_str("]}}\n");
        let _ = out.write_all(buf.as_bytes());
    } else {
        let first = &skipped[0];
        eprintln!(
            "tzip {}: incomplete — {} of {} entries skipped (first: {} — {})",
            cmd,
            skipped.len(),
            total,
            first.where_,
            first.reason,
        );
    }
    strict
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
