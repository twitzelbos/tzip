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
