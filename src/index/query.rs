//! SQL query dispatcher — Phase 5 of the Elastic feature-parity
//! roadmap.
//!
//! `tzip query --sql "<SQL>" archive.zip` reads the
//! `.tzip/columnar.v1.parquet` entry out of the archive, stages it
//! to a temp file, and runs the user's query through the system
//! `duckdb` CLI with the Parquet file registered as the `tzip`
//! view. DuckDB covers everything Elastic's aggregation DSL does —
//! grouping, nested `COUNT(DISTINCT ...)`, date histograms via
//! `date_trunc`, percentiles, joins across sidecars — and avoids
//! us reinventing a query engine inside tzip.
//!
//! ## Why external, not bundled
//!
//! The `duckdb` Rust crate with `bundled` compiles DuckDB C++ from
//! source (minutes of build time, ~50 MB added to the binary).
//! Shelling out lets tzip ship lean and keeps us on DuckDB's own
//! release train. The error message points at `brew install
//! duckdb` / `apt install duckdb` when the CLI isn't found.
//!
//! ## Output format
//!
//! Default: DuckDB's table box-drawing output (same as running the
//! query interactively). Pass `--json` to get one JSON record per
//! row, suitable for piping into jq. Pass `--csv` for CSV with a
//! header row.
//!
//! ## Row-group pruning (PR #72 + follow-ups)
//!
//! Before invoking DuckDB on a remote archive, we try to range-
//! read only the Parquet row groups whose min/max stats could
//! match the SQL's WHERE predicates. The pipeline is:
//!
//!   1. Parse the SQL into a flat `col OP constant` predicate
//!      list via `parquet_prune::extract_conjunctive_predicates`.
//!      Anything the mini-parser can't handle (joins, `OR`,
//!      subqueries, functions on columns) → `None` → full-body
//!      fallback.
//!   2. Range-read the Parquet footer (tens of KB) off each shard
//!      sidecar via the regular `sources::` Range-GET stack.
//!   3. Compare each row group's column statistics against the
//!      predicates. Prove-exclusion drops the row group;
//!      ambiguous or stats-missing keeps it. Pruning is a pure
//!      speed optimisation — DuckDB still runs the full WHERE on
//!      whatever we hand it, so a mistakenly kept row group only
//!      wastes CPU, never corrupts results.
//!   4. Compose a Parquet with only the surviving row groups to
//!      the temp file DuckDB reads.
//!
//! ### Why not just delegate everything to DuckDB?
//!
//! The predicate extractor and row-group pruner are *not*
//! reimplementing a query optimiser — they're a **client-side
//! Parquet Range-fetch planner**. We have to decide which bytes
//! to pull BEFORE DuckDB runs, because:
//!
//! - DuckDB's `httpfs` extension can pushdown predicates into a
//!   Parquet file *at a URL*, but our Parquet sidecar is a byte
//!   range inside a ZIP, not a file at a URL.
//! - By the time we've handed DuckDB a local temp file, the
//!   network cost is sunk — DuckDB's own pruning can only save
//!   CPU, not bytes.
//!
//! So the client-side pruner saves network I/O that no downstream
//! engine can recover, and DuckDB retains ownership of everything
//! above that: schema binding, joins across shards (via
//! `read_parquet([...])`), aggregates, percentiles, sort/limit.
//! The division of labour is deliberate.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::zipreader::Archive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryFormat {
    Box,
    Json,
    Csv,
}

pub struct QueryOpts {
    /// Archive specs: one or more local paths, `http(s)://` URLs,
    /// or `s3://bucket/key.zip` URLs; S3 globs like
    /// `s3://bucket/shards/*.zip` are expanded via `ListObjectsV2`.
    /// Multiple shards are UNION'd via DuckDB's `read_parquet([...])`
    /// — the Phase 7 horizontal-shard fan-out.
    pub archives: Vec<String>,
    pub sql: String,
    pub format: QueryFormat,
    pub password: Option<String>,
    /// When set, index entries (including each shard's columnar
    /// sidecar) are kept in an on-disk LRU under `spec.dir`.
    /// Repeat queries then hit the cache instead of re-fetching.
    pub cache: Option<crate::indexcache::IndexCacheSpec>,
}

/// Dispatch one SQL query across the columnar sidecars of one or
/// more archives.
///
/// Each shard's `.tzip/columnar.v1.parquet` entry is extracted to a
/// temp file (`tempfile::NamedTempFile`, cleaned up on drop), then
/// all temp paths are handed to DuckDB via `read_parquet([...])` —
/// DuckDB UNION'd Parquet reads cover Phase 7's "horizontal shard
/// parallelism" row: the merge happens inside the query engine, not
/// in tzip's Rust.
pub fn run(opts: QueryOpts) -> Result<()> {
    let archives = crate::shards::expand(&opts.archives)?;
    if archives.is_empty() {
        bail!("no archives matched the given patterns");
    }

    // Open + extract each shard's sidecar BEFORE probing for
    // `duckdb`. A missing sidecar on any shard is a create-side
    // problem; missing duckdb is a one-time install. Surfacing the
    // create-side error first saves the user "install duckdb, retry,
    // still broken" trips.
    let cache_arc = match &opts.cache {
        Some(spec) => Some(spec.open()?),
        None => None,
    };
    // ChatGPT review #4: parse the user's SQL once. If it fits
    // the simple-WHERE grammar our pruner handles, we can
    // range-read only the row groups whose stats might match.
    // On anything the pruner can't parse (joins, OR, subqueries,
    // …) `extract_conjunctive_predicates` returns None and we
    // drop back to the full-extract path. Pruning is pure speed;
    // soundness-critical corners all vote "keep the row group".
    let preds = crate::parquet_prune::extract_conjunctive_predicates(&opts.sql);
    let mut temps: Vec<tempfile::NamedTempFile> = Vec::with_capacity(archives.len());
    let mut total_rgs: usize = 0;
    let mut kept_rgs: usize = 0;
    for spec in &archives {
        let mut archive =
            crate::reader::open_archive_with_cache(spec, cache_arc.clone())
                .with_context(|| format!("open archive {}", spec))?;
        if let Some(pw) = opts.password.as_deref() {
            archive.attach_password(pw.to_string());
        }
        let ranges = archive.columnar_segment_ranges();
        if ranges.is_empty() {
            bail!(
                "archive {} has no columnar sidecar entry ({}). \
                 Re-create with `tzip create --write-columnar schema.json` to add one.",
                spec,
                crate::columnar::INDEX_ENTRY_NAME,
            );
        }

        for (seg_idx, maybe_range) in ranges.iter().enumerate() {
            let tmp = tempfile::Builder::new()
                .prefix("tzip-columnar-")
                .suffix(".parquet")
                .tempfile()
                .context("create temp file for columnar sidecar")?;

            // Try the range-read prune path when (a) the SQL
            // parsed into a non-trivial predicate list, (b) the
            // sidecar entry is Store + unencrypted (otherwise
            // its body bytes aren't the raw Parquet). Fall back
            // to the full-body extract on any ambiguity.
            let did_prune = match (&preds, maybe_range) {
                (Some(ps), Some((_, body_off, body_len))) if !ps.is_empty() => {
                    try_prune_segment(spec, *body_off, *body_len, ps, tmp.path())
                        .context("range-read prune")?
                }
                _ => None,
            };

            match did_prune {
                Some((kept, total)) => {
                    kept_rgs += kept;
                    total_rgs += total;
                }
                None => {
                    // Fallback: read the whole body and write
                    // to the temp file. For segments past the
                    // first we still do one full body read, but
                    // the cache will reuse it on repeat queries.
                    let payloads = archive.columnar_segment_bodies();
                    let payload = payloads
                        .into_iter()
                        .nth(seg_idx)
                        .ok_or_else(|| {
                            anyhow::anyhow!("columnar segment {} vanished", seg_idx)
                        })?;
                    tmp.as_file()
                        .write_all_at_zero(&payload)
                        .context("write columnar sidecar to temp file")?;
                }
            }
            temps.push(tmp);
        }
    }
    if preds.as_ref().is_some_and(|p| !p.is_empty()) && total_rgs > 0 {
        eprintln!(
            "tzip: query prune — {}/{} row groups kept after WHERE push-down",
            kept_rgs, total_rgs
        );
    }

    if which_duckdb().is_none() {
        let first = archives.first().map(|s| s.as_str()).unwrap_or("archive.zip");
        bail!(
            "`duckdb` CLI not found on PATH. Install it to use `tzip query --sql`:\n\
             - macOS:  brew install duckdb\n\
             - Linux:  apt install duckdb  (or download from https://duckdb.org/docs/installation/)\n\
             Alternatively, extract the sidecar with `tzip x {} {}` and query it directly.",
            first,
            crate::columnar::INDEX_ENTRY_NAME,
        );
    }

    let parquet_paths: Vec<&Path> = temps.iter().map(|t| t.path()).collect();
    run_duckdb(&parquet_paths, &opts.sql, opts.format)
}

/// Return the base `.tzip/columnar.v1.parquet` body and every
/// `.segN` sibling in segment order. Password is unused for now —
/// columnar sidecars are always Store-method unencrypted entries —
/// but carried in the signature so future AES-sealed sidecars
/// won't need a call-site refactor.
#[allow(dead_code)]
fn extract_columnar_segment_bodies(
    archive: &mut Archive,
    _password: Option<&str>,
) -> Result<Vec<Vec<u8>>> {
    Ok(archive.columnar_segment_bodies())
}

/// Open a fresh `ReadSeek` source for `spec`, wrap the sidecar's
/// `[body_off, body_off + body_len)` byte range in a
/// `ChunkReader`, prune row groups, and write the pruned
/// Parquet to `tmp`. Returns:
/// - `Ok(Some((kept, total)))` on a successful partial prune.
/// - `Ok(None)` when every row group matches (caller does the
///   full-body extract so the final file is byte-identical to
///   what DuckDB would read anyway).
fn try_prune_segment(
    spec: &str,
    body_off: u64,
    body_len: u64,
    preds: &[crate::parquet_prune::Predicate],
    tmp: &Path,
) -> Result<Option<(usize, usize)>> {
    let source =
        crate::reader::open_source_for_with_hint(spec, None).with_context(|| {
            format!("open raw source for prune range-read on {}", spec)
        })?;
    let slice = crate::parquet_prune::ArchiveEntrySlice::new(source, body_off, body_len);
    let report = crate::parquet_prune::prune_from_slice(slice, preds, tmp)?;
    match report.pruned {
        Some(kept) => Ok(Some((kept, report.total))),
        None => Ok(None),
    }
}

/// Portable "write at offset 0" — using plain `Write::write_all` on
/// a `NamedTempFile::as_file()` requires `&mut File`, but we only
/// have `&File`. Rather than fight the borrow, go through
/// `write_at` (unix) / positional write.
trait WriteAllAt {
    fn write_all_at_zero(&self, buf: &[u8]) -> std::io::Result<()>;
}

#[cfg(unix)]
impl WriteAllAt for std::fs::File {
    fn write_all_at_zero(&self, buf: &[u8]) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.write_all_at(buf, 0)
    }
}

#[cfg(windows)]
impl WriteAllAt for std::fs::File {
    fn write_all_at_zero(&self, buf: &[u8]) -> std::io::Result<()> {
        use std::os::windows::fs::FileExt;
        // seek_write doesn't guarantee writing the full buffer in one
        // call, so loop.
        let mut written = 0usize;
        while written < buf.len() {
            let n = self.seek_write(&buf[written..], written as u64)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "seek_write returned 0",
                ));
            }
            written += n;
        }
        Ok(())
    }
}

/// Look up the `duckdb` binary via PATH. `which(1)` on mac/linux.
fn which_duckdb() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("duckdb");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run_duckdb(parquet_paths: &[&Path], sql: &str, format: QueryFormat) -> Result<()> {
    // Register all shard sidecars as a single view named `tzip`.
    // `read_parquet(['a.parquet','b.parquet',...])` UNION-reads the
    // files with aligned schemas — the standard DuckDB pattern for
    // multi-file tables. One shard is just a single-element list.
    let mut quoted = String::new();
    for (i, p) in parquet_paths.iter().enumerate() {
        if i > 0 {
            quoted.push_str(",");
        }
        let s = p
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("parquet temp path is not UTF-8"))?
            .replace('\'', "''");
        quoted.push('\'');
        quoted.push_str(&s);
        quoted.push('\'');
    }
    let setup = format!(
        "{format_cmd}\nCREATE VIEW tzip AS SELECT * FROM read_parquet([{paths}]);\n",
        format_cmd = match format {
            QueryFormat::Box => ".mode box",
            QueryFormat::Json => ".mode json",
            QueryFormat::Csv => ".mode csv",
        },
        paths = quoted,
    );
    let script = format!("{setup}{sql};\n");

    // `-no-agent` suppresses DuckDB 2.0's "agent mode" heuristic
    // (triggered by the `AI_AGENT` env var or a non-tty stdout),
    // which otherwise prints a banner to stdout and can rewrap
    // results. `tzip query`'s output is structured; we don't want
    // the banner.
    let mut child = Command::new("duckdb")
        .arg("-no-agent")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn duckdb")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("duckdb stdin not captured"))?;
        stdin
            .write_all(script.as_bytes())
            .context("write SQL script to duckdb stdin")?;
    }
    let status = child.wait().context("wait for duckdb")?;
    if !status.success() {
        bail!("duckdb exited with status {}", status);
    }
    Ok(())
}

