//! End-to-end tests for Phase 5b: `tzip u --write-columnar` emits
//! `.tzip/columnar.v1.parquet.segN`; `tzip query` reads base plus
//! every segment via DuckDB's `read_parquet([..])` UNION so the
//! appended rows show up without a full archive rebuild.
//!
//! Query-side tests skip gracefully when `duckdb` isn't on PATH so
//! CI boxes without DuckDB still pass the rest.

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

fn make_corpus(root: &Path, dir: &str, n: usize, status_base: i32) {
    let logs = root.join(dir).join("logs");
    fs::create_dir_all(&logs).unwrap();
    for i in 0..n {
        fs::write(
            logs.join(format!("log_{:04}.json", i)),
            format!(r#"{{"status_code":{},"user":"u{}"}}"#, status_base + (i as i32 % 3), i),
        )
        .unwrap();
    }
}

fn write_schema(root: &Path) -> PathBuf {
    let p = root.join("schema.json");
    fs::write(
        &p,
        br#"{
            "fields": [
                { "name": "status_code", "type": "int64",  "path": ".status_code" },
                { "name": "user",        "type": "string", "path": ".user" }
            ]
        }"#,
    )
    .unwrap();
    p
}

#[test]
fn append_emits_columnar_segment_with_auto_loaded_schema() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", 6, 200); // 2× each of 200/201/202
    let schema = write_schema(tmp.path());
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "a/logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            schema.to_str().unwrap(),
            "-q",
        ],
    );
    make_corpus(tmp.path(), "b", 3, 500); // 1× each of 500/501/502
    // Bare `--write-columnar` auto-loads the schema from the base
    // sidecar's Parquet metadata — no second schema path needed.
    run(
        tmp.path(),
        &[
            "u",
            "out.zip",
            "b/logs",
            "--index-terms",
            "*.json",
            "--write-columnar",
            "-q",
        ],
    );
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        text.contains(".tzip/columnar.v1.parquet\n") || text.contains(".tzip/columnar.v1.parquet "),
        "base sidecar missing: {}",
        text
    );
    assert!(
        text.contains(".tzip/columnar.v1.parquet.seg1"),
        "seg1 missing after append: {}",
        text
    );
}

#[test]
fn two_appends_produce_seg1_and_seg2() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", 3, 200);
    let schema = write_schema(tmp.path());
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "a/logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            schema.to_str().unwrap(),
            "-q",
        ],
    );
    make_corpus(tmp.path(), "b", 3, 500);
    run(
        tmp.path(),
        &["u", "out.zip", "b/logs", "--index-terms", "*.json", "--write-columnar", "-q"],
    );
    make_corpus(tmp.path(), "c", 3, 800);
    run(
        tmp.path(),
        &["u", "out.zip", "c/logs", "--index-terms", "*.json", "--write-columnar", "-q"],
    );
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains(".tzip/columnar.v1.parquet.seg1"));
    assert!(text.contains(".tzip/columnar.v1.parquet.seg2"));
}

#[test]
fn bare_write_columnar_errors_when_no_base_sidecar() {
    // Archive has no columnar sidecar; `--write-columnar` without
    // `--columnar-schema` has nothing to auto-load from.
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", 3, 200);
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "--index-terms", "*.json", "-q"],
    );
    make_corpus(tmp.path(), "b", 3, 500);
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&[
            "u",
            "out.zip",
            "b/logs",
            "--index-terms",
            "*.json",
            "--write-columnar",
            "-q",
        ])
        .output()
        .expect("run tzip");
    assert!(!out.status.success(), "append should refuse without a base to auto-load from");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("auto-load") || err.contains("columnar"),
        "unhelpful error: {}",
        err
    );
}

#[test]
fn query_unions_base_and_segments_via_duckdb() {
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", 6, 200);
    let schema = write_schema(tmp.path());
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "a/logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--write-columnar",
            schema.to_str().unwrap(),
            "-q",
        ],
    );
    make_corpus(tmp.path(), "b", 6, 500);
    run(
        tmp.path(),
        &["u", "out.zip", "b/logs", "--index-terms", "*.json", "--write-columnar", "-q"],
    );
    make_corpus(tmp.path(), "c", 3, 800);
    run(
        tmp.path(),
        &["u", "out.zip", "c/logs", "--index-terms", "*.json", "--write-columnar", "-q"],
    );
    // 6 + 6 + 3 = 15 rows across base + seg1 + seg2.
    let out = run(
        tmp.path(),
        &["query", "--format", "csv", "-s", "SELECT COUNT(*) AS n FROM tzip", "out.zip"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("\n15\n") || text.lines().any(|l| l == "15"),
        "COUNT(*) across segments wrong: {}",
        text
    );

    // Distinct status buckets: base 200/201/202, seg1 500/501/502,
    // seg2 800/801/802.
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
    for s in &[200, 201, 202, 500, 501, 502, 800, 801, 802] {
        assert!(
            text.contains(&format!("{},", s)),
            "status_code {} missing from merged query: {}",
            s,
            text
        );
    }
}

#[test]
fn query_works_on_archive_with_only_base_sidecar() {
    // Backwards-compat: Phase-5 archives (no segments) should
    // still query identically — the Phase-5b code path is a
    // single-element list of (base,) = exactly the old behaviour.
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", 9, 200);
    let schema = write_schema(tmp.path());
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "a/logs",
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
        &["query", "--format", "csv", "-s", "SELECT COUNT(*) FROM tzip", "out.zip"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("\n9\n") || text.lines().any(|l| l == "9"),
        "base-only count wrong: {}",
        text
    );
}
