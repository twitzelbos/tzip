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
    /// Archive spec: local path, `http(s)://` URL, or `s3://` URL.
    pub archive: String,
    pub sql: String,
    pub format: QueryFormat,
    pub password: Option<String>,
    /// When set, index entries (including the columnar sidecar)
    /// extracted from this archive are kept in an on-disk LRU under
    /// `spec.dir`. Repeat queries against the same remote archive
    /// then hit the cache instead of re-fetching.
    pub cache: Option<crate::indexcache::IndexCacheSpec>,
}

/// Dispatch one SQL query against an archive's columnar sidecar.
///
/// The sidecar lives inside the ZIP, so we stage it to a temp
/// Parquet file before invoking `duckdb`. The temp file is removed
/// via `tempfile::NamedTempFile` on drop; DuckDB reads the file in
/// one shot so there's no lifetime issue.
pub fn run(opts: QueryOpts) -> Result<()> {
    // Open + extract the sidecar BEFORE probing for `duckdb`. A
    // missing sidecar is a create-side problem the user fixes by
    // rebuilding; missing duckdb is a one-time install. If both
    // apply, surface the create-side error first — otherwise users
    // follow the install hint and still hit the sidecar error on
    // the next run.
    let cache_arc = match &opts.cache {
        Some(spec) => Some(spec.open()?),
        None => None,
    };
    let mut archive = crate::reader::open_archive_with_cache(&opts.archive, cache_arc)
        .with_context(|| format!("open archive {}", opts.archive))?;
    let payload = extract_columnar_entry(&mut archive, opts.password.as_deref())?;

    if which_duckdb().is_none() {
        bail!(
            "`duckdb` CLI not found on PATH. Install it to use `tzip query --sql`:\n\
             - macOS:  brew install duckdb\n\
             - Linux:  apt install duckdb  (or download from https://duckdb.org/docs/installation/)\n\
             Alternatively, extract the sidecar with `tzip x {} {}` and query it directly.",
            opts.archive,
            crate::columnar::INDEX_ENTRY_NAME,
        );
    }

    // Stage the Parquet bytes to a temp file. DuckDB reads the file
    // through its Parquet reader; passing it on stdin isn't an
    // option because Parquet is seekable-only.
    let tmp = tempfile::Builder::new()
        .prefix("tzip-columnar-")
        .suffix(".parquet")
        .tempfile()
        .context("create temp file for columnar sidecar")?;
    tmp.as_file()
        .write_all_at_zero(&payload)
        .context("write columnar sidecar to temp file")?;

    run_duckdb(tmp.path(), &opts.sql, opts.format)
}

fn extract_columnar_entry(archive: &mut Archive, password: Option<&str>) -> Result<Vec<u8>> {
    let name = crate::columnar::INDEX_ENTRY_NAME;
    let idx = archive
        .lookup_entry_idx(name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "archive has no columnar sidecar entry ({}). \
                 Re-create with `tzip create --write-columnar schema.json` to add one.",
                name
            )
        })?;
    archive.lfh_total_size_of(idx)?;
    let entry = archive.entries()[idx].clone();
    let mut out: Vec<u8> = Vec::with_capacity(entry.uncomp_size as usize);
    crate::reader::stream_entry_to(archive.source_mut(), &entry, password, &mut out)
        .with_context(|| format!("read columnar sidecar entry {}", name))?;
    Ok(out)
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

fn run_duckdb(parquet_path: &Path, sql: &str, format: QueryFormat) -> Result<()> {
    // Register the Parquet file as a view named `tzip` so the user's
    // SQL can use `FROM tzip`. The initial dot-command sets the
    // output format for everything that follows on the same stdin.
    let setup = format!(
        "{format_cmd}\nCREATE VIEW tzip AS SELECT * FROM read_parquet('{path}');\n",
        format_cmd = match format {
            QueryFormat::Box => ".mode box",
            QueryFormat::Json => ".mode json",
            QueryFormat::Csv => ".mode csv",
        },
        path = parquet_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("parquet temp path is not UTF-8"))?
            .replace('\'', "''"),
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

