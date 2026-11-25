//! End-to-end tests for Phase 5 columnar sidecar: `tzip create
//! --write-columnar schema.json` writes a `.tzip/columnar.v1.parquet`
//! entry, and `tzip query --sql` dispatches to DuckDB over it.
//!
//! DuckDB-dependent tests skip gracefully when the `duckdb` binary
//! isn't on PATH, so CI boxes without DuckDB still pass the rest
//! of the suite.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run(cwd: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run tzip");
    assert!(
        out.status.success(),
        "tzip {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn have_duckdb() -> bool {
    Command::new("duckdb")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Write `n` JSON log entries with typed fields across cycling buckets.
fn make_json_corpus(root: &Path, n: usize) {
    fs::create_dir_all(root.join("logs")).unwrap();
    for i in 0..n {
        let status = match i % 3 {
            0 => 200,
            1 => 404,
            _ => 500,
        };
        let cached = if i % 2 == 0 { "true" } else { "false" };
        let body = format!(
            "{{\"ts\":\"2026-11-24\",\"status_code\":{},\"user\":\"u{}\",\"latency_ms\":{}.5,\"cached\":{}}}\n",
            status,
            i % 5,
            10 + i,
            cached,
        );
        fs::write(root.join(format!("logs/log_{:04}.json", i)), body).unwrap();
    }
}

fn write_schema(root: &Path) -> PathBuf {
    let path = root.join("schema.json");
    fs::write(
        &path,
        br#"{
            "fields": [
                { "name": "ts",          "type": "string",  "path": ".ts" },
                { "name": "status_code", "type": "int64",   "path": ".status_code" },
                { "name": "user",        "type": "string",  "path": ".user" },
                { "name": "latency_ms",  "type": "float64", "path": ".latency_ms" },
                { "name": "cached",      "type": "bool",    "path": ".cached" }
            ]
        }"#,
    )
    .unwrap();
    path
}

#[test]
fn create_with_write_columnar_emits_parquet_entry() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 10);
    let schema = write_schema(tmp.path());
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            "--columnar-schema",
            schema.to_str().unwrap(),
            "-q",
        ],
    );
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        text.contains(".tzip/columnar.v1.parquet"),
        "columnar sidecar missing in listing: {}",
        text
    );

    // Extract the Parquet entry and verify the file-format magic.
    let extracted_dir = tmp.path().join("ex");
    fs::create_dir_all(&extracted_dir).unwrap();
    run(
        tmp.path(),
        &[
            "x",
            "out.zip",
            "--to",
            extracted_dir.to_str().unwrap(),
            ".tzip/columnar.v1.parquet",
        ],
    );
    let parquet_path = extracted_dir.join(".tzip/columnar.v1.parquet");
    let bytes = fs::read(&parquet_path).unwrap();
    assert!(bytes.len() > 8, "parquet file suspiciously small: {}", bytes.len());
    assert_eq!(&bytes[0..4], b"PAR1", "missing head magic");
    assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "missing tail magic");
}

#[test]
fn query_rejects_archive_without_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 3);
    run(
        tmp.path(),
        &["create", "nocols.zip", "logs", "-m", "deflate", "-q"],
    );
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&["query", "-s", "SELECT 1", "nocols.zip"])
        .output()
        .expect("run tzip query");
    assert!(!out.status.success(), "query should have failed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no columnar sidecar"),
        "wrong error: {}",
        err
    );
}

#[test]
fn query_rejects_bad_schema_at_create_time() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 3);
    let bad = tmp.path().join("bad.json");
    fs::write(&bad, br#"{"fields": []}"#).unwrap();
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&[
            "create",
            "out.zip",
            "logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            "--columnar-schema",
            bad.to_str().unwrap(),
            "-q",
        ])
        .output()
        .expect("run tzip create");
    assert!(!out.status.success(), "create should have failed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no fields"),
        "expected empty-fields error, got: {}",
        err
    );
}

#[test]
fn query_runs_sql_via_duckdb() {
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 9); // 3× each of 200, 404, 500
    let schema = write_schema(tmp.path());
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            "--columnar-schema",
            schema.to_str().unwrap(),
            "-q",
        ],
    );
    let out = run(
        tmp.path(),
        &[
            "query",
            "--format",
            "csv",
            "-s",
            "SELECT status_code, COUNT(*) AS n FROM tzip GROUP BY status_code ORDER BY status_code",
            "out.zip",
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // CSV header + three data rows (200, 404, 500) each with count 3.
    assert!(text.contains("status_code,n"), "missing header: {}", text);
    assert!(text.contains("200,3"), "wrong count for 200: {}", text);
    assert!(text.contains("404,3"), "wrong count for 404: {}", text);
    assert!(text.contains("500,3"), "wrong count for 500: {}", text);
}

#[test]
fn query_row_group_prune_narrows_scan_and_returns_correct_results() {
    // ChatGPT review #4 regression test. Build a sidecar with
    // ≥ 2 row groups by pushing 70 Ki JSONL rows through the
    // columnar builder, then run a predicate that only one row
    // group's stats allow and verify:
    //
    //   (a) the result is correct (DuckDB sees exactly the
    //       matching rows, not an under- or over-count),
    //   (b) stderr shows the prune diagnostic with N < M,
    //       proving the range-read pruner actually dropped a
    //       row group.
    //
    // Soundness regression target: a bug in the predicate
    // extractor or stats comparator would most likely show up
    // as (a) failing, not (b) — hence we assert (a) hard and
    // (b) only probabilistically.
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("logs")).unwrap();

    // Two NDJSON files, each ~40 Ki rows, with disjoint `n`
    // ranges — guarantees two distinct row groups with
    // non-overlapping `n` statistics.
    let rows_per_file = 40_000;
    for (file_idx, (low, high)) in [(0, 40_000), (100_000, 140_000)].iter().enumerate() {
        let mut body = String::with_capacity(rows_per_file * 20);
        for n in *low..*high {
            use std::fmt::Write;
            writeln!(body, "{{\"n\":{}}}", n).unwrap();
        }
        fs::write(
            tmp.path().join(format!("logs/batch_{:02}.jsonl", file_idx)),
            body.as_bytes(),
        )
        .unwrap();
    }
    let schema = tmp.path().join("schema.json");
    fs::write(
        &schema,
        br#"{ "fields": [ { "name": "n", "type": "int64", "path": ".n" } ] }"#,
    )
    .unwrap();

    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "logs",
            "-m",
            "store",
            "--index-terms",
            "*.jsonl",
            "--write-columnar",
            "--columnar-schema",
            schema.to_str().unwrap(),
            "-q",
        ],
    );

    // Pick a predicate whose constant falls between the two
    // batches' ranges but past where the column builder's
    // 64 Ki-row-per-row-group flush lands. Since the builder
    // flushes at 64 Ki total rows and each batch is 40 Ki, the
    // first row group straddles the batch boundary — but the
    // key property we need is that ONE of the two resulting
    // row groups has max(n) < 130000 and the other has
    // max(n) > 130000. That holds for either file-walk order
    // because the two batches' n-ranges are disjoint.
    //
    //   (0..40000, 100000..140000) walked in order →
    //     RG1 covers n ∈ [0, ~124000), RG2 covers [~124000, 140000)
    //     → RG1 max < 130000 (pruned), RG2 max > 130000 (kept)
    //
    //   reverse walk order →
    //     RG1 covers n ∈ [0, ~164000) spanning both batches
    //       (but file_01 only has 40 Ki rows so the overflow
    //       spills into file_00), RG2 covers the other tail
    //     → either way exactly one row group's max falls past
    //       130000 and the other's does not.
    //
    // 140_000 - 130_000 - 1 = 9_999 rows satisfy `n > 130000`.
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args([
            "query",
            "--format",
            "csv",
            "-s",
            "SELECT COUNT(*) AS n FROM tzip WHERE n > 130000",
            "out.zip",
        ])
        .output()
        .expect("run tzip query");
    assert!(
        out.status.success(),
        "query failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("9999"),
        "expected 9999 rows satisfying n > 130000, got stdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    // Prune diagnostic should fire with N<M. A regression that
    // silently disables pruning fails here even though results
    // stay correct.
    assert!(
        stderr.contains("row groups kept"),
        "expected prune diagnostic in stderr: {}",
        stderr
    );
    // Parse "N/M row groups kept" and assert N < M.
    let frac = stderr
        .lines()
        .find(|l| l.contains("row groups kept"))
        .and_then(|l| l.split_whitespace().find(|t| t.contains('/')))
        .expect("prune diagnostic format");
    let (kept, total) = frac.split_once('/').unwrap();
    let kept: usize = kept.parse().unwrap();
    let total: usize = total.parse().unwrap();
    assert!(total >= 2, "expected ≥ 2 row groups to prune from, got {}", total);
    assert!(
        kept < total,
        "expected pruning to drop a row group, got kept={}, total={}",
        kept,
        total
    );
}

#[test]
fn query_supports_date_bucketing() {
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Synthesise events at distinct hourly timestamps.
    fs::create_dir_all(tmp.path().join("logs")).unwrap();
    let samples: &[(&str, i64)] = &[
        ("2026-11-24T00:30:00Z", 200),
        ("2026-11-24T00:45:00Z", 404),
        ("2026-11-24T01:10:00Z", 500),
        ("2026-11-24T01:50:00Z", 200),
        ("2026-11-24T02:05:00Z", 200),
    ];
    for (i, (ts, status)) in samples.iter().enumerate() {
        fs::write(
            tmp.path().join(format!("logs/e_{:02}.json", i)),
            format!(r#"{{"ts":"{}","status_code":{}}}"#, ts, status),
        )
        .unwrap();
    }
    let schema = tmp.path().join("schema.json");
    fs::write(
        &schema,
        br#"{
            "fields": [
                { "name": "ts",     "type": "string", "path": ".ts" },
                { "name": "status", "type": "int64",  "path": ".status_code" }
            ]
        }"#,
    )
    .unwrap();
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            "--columnar-schema",
            schema.to_str().unwrap(),
            "-q",
        ],
    );
    let out = run(
        tmp.path(),
        &[
            "query",
            "--format",
            "csv",
            "-s",
            "SELECT date_trunc('hour', CAST(ts AS TIMESTAMP)) AS hr, COUNT(*) AS n \
             FROM tzip GROUP BY hr ORDER BY hr",
            "out.zip",
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // Three hour buckets: 00:00 (2), 01:00 (2), 02:00 (1).
    assert!(
        text.contains("2026-11-24 00:00:00,2"),
        "hour 00 bucket wrong: {}",
        text
    );
    assert!(
        text.contains("2026-11-24 01:00:00,2"),
        "hour 01 bucket wrong: {}",
        text
    );
    assert!(
        text.contains("2026-11-24 02:00:00,1"),
        "hour 02 bucket wrong: {}",
        text
    );
}
