//! Phase 5c: `tzip create --write-columnar` with no
//! `--columnar-schema` auto-infers the schema from the first N
//! JSON entries matching `--index-terms`.

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

fn make_corpus(root: &Path, n: usize) {
    let logs = root.join("logs");
    fs::create_dir_all(&logs).unwrap();
    for i in 0..n {
        let status = match i % 3 {
            0 => 200,
            1 => 404,
            _ => 500,
        };
        fs::write(
            logs.join(format!("log_{:04}.json", i)),
            format!(
                r#"{{"ts":"2026-11-25","status_code":{},"user":"u{}","latency_ms":{}.5}}"#,
                status,
                i % 5,
                10 + i
            ),
        )
        .unwrap();
    }
}

#[test]
fn bare_write_columnar_infers_and_emits_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 10);
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
            "-q",
        ],
    );
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        text.contains(".tzip/columnar.v1.parquet"),
        "auto-inferred sidecar missing: {}",
        text
    );
}

#[test]
fn bare_write_columnar_errors_without_index_terms() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 3);
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&["create", "out.zip", "logs", "-m", "deflate", "--write-columnar", "-q"])
        .output()
        .expect("run tzip");
    assert!(
        !out.status.success(),
        "auto-infer without --index-terms should fail"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("auto-infer") && err.contains("index-terms"),
        "unhelpful error: {}",
        err
    );
}

#[test]
fn explicit_columnar_schema_overrides_auto_infer() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 5);
    let schema = tmp.path().join("schema.json");
    fs::write(
        &schema,
        br#"{"fields":[
            {"name":"ts","type":"string","path":".ts"},
            {"name":"status_code","type":"int64","path":".status_code"}
        ]}"#,
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
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    assert!(String::from_utf8_lossy(&listing.stdout).contains(".tzip/columnar.v1.parquet"));
}

#[test]
fn inferred_schema_queries_correctly_via_duckdb() {
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 9); // 3× 200, 3× 404, 3× 500
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
    assert!(text.contains("200,3"), "200 count wrong: {}", text);
    assert!(text.contains("404,3"), "404 count wrong: {}", text);
    assert!(text.contains("500,3"), "500 count wrong: {}", text);
}

#[test]
fn infer_widens_int_to_float_in_pipeline() {
    // Mixed int/float samples under one column name should land as
    // float64 — exercised via `tzip query` returning decimals.
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let logs = tmp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    // First 5 entries: integer latency. Next 5: decimal latency.
    for i in 0..5 {
        fs::write(logs.join(format!("a_{}.json", i)), format!(r#"{{"latency":{}}}"#, i * 10)).unwrap();
    }
    for i in 0..5 {
        fs::write(logs.join(format!("b_{}.json", i)), format!(r#"{{"latency":{}.5}}"#, i)).unwrap();
    }
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
            "-q",
        ],
    );
    // SUM of latency: 0+10+20+30+40 + 0.5+1.5+2.5+3.5+4.5 = 100 + 12.5 = 112.5
    let out = run(
        tmp.path(),
        &[
            "query",
            "--format",
            "csv",
            "-s",
            "SELECT SUM(latency) AS s FROM tzip",
            "out.zip",
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("112.5"),
        "widened float sum wrong: {}",
        text
    );
}
