//! End-to-end tests for Phase 7 shard fan-out.  `tzip aggregate`
//! sums facet counts across every archive the shell / glob
//! expander hands it; `tzip query` UNIONs the columnar sidecars
//! inside DuckDB via `read_parquet([..])`.  DuckDB-dependent tests
//! skip gracefully when the `duckdb` CLI isn't on PATH so hosts
//! without it still pass the rest.

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

/// Build a shard with `n` JSON log entries. `tag` mixes into the
/// user field so different shards produce distinguishable payloads.
fn build_shard(root: &Path, dir: &str, n: usize, tag: &str) -> PathBuf {
    let logs = root.join(dir).join("logs");
    fs::create_dir_all(&logs).unwrap();
    for i in 0..n {
        let status = match i % 3 {
            0 => 200,
            1 => 404,
            _ => 500,
        };
        fs::write(
            logs.join(format!("{}_{:04}.json", tag, i)),
            format!(
                r#"{{"status_code":{},"user":"{}-{}"}}"#,
                status,
                tag,
                i % 4
            ),
        )
        .unwrap();
    }
    let schema = root.join("schema.json");
    if !schema.exists() {
        fs::write(
            &schema,
            br#"{
                "fields": [
                    { "name": "status_code", "type": "int64",  "path": ".status_code" },
                    { "name": "user",        "type": "string", "path": ".user" }
                ]
            }"#,
        )
        .unwrap();
    }
    let out = root.join(format!("{}.zip", dir));
    run(
        root,
        &[
            "create",
            out.file_name().unwrap().to_str().unwrap(),
            &format!("{}/logs", dir),
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--facet",
            ".status_code",
            "--write-columnar",
            "--columnar-schema",
            schema.file_name().unwrap().to_str().unwrap(),
            "-q",
        ],
    );
    out
}

#[test]
fn aggregate_sums_counts_across_shards() {
    let tmp = tempfile::tempdir().unwrap();
    // Three shards, 9 entries each → 27 total.  Each shard has
    // 3 × 200, 3 × 404, 3 × 500.  Merged: 9 of each, 3 distinct,
    // 27 total.
    let a = build_shard(tmp.path(), "a", 9, "a");
    let b = build_shard(tmp.path(), "b", 9, "b");
    let c = build_shard(tmp.path(), "c", 9, "c");
    let out = run(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            c.to_str().unwrap(),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("9  200"), "200 sum wrong: {}", text);
    assert!(text.contains("9  404"), "404 sum wrong: {}", text);
    assert!(text.contains("9  500"), "500 sum wrong: {}", text);
    assert!(
        text.contains("3 distinct value(s), 27 total entries across 3 shards"),
        "footer wrong: {}",
        text
    );
}

#[test]
fn aggregate_json_format_across_shards() {
    let tmp = tempfile::tempdir().unwrap();
    let a = build_shard(tmp.path(), "a", 6, "a"); // 2× each
    let b = build_shard(tmp.path(), "b", 6, "b"); // 2× each
    let out = run(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--json",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // JSON: one `{"value": "200", "count": 4}` per distinct value.
    assert!(text.contains(r#""value":"200","count":4"#), "json 200: {}", text);
    assert!(text.contains(r#""value":"404","count":4"#), "json 404: {}", text);
    assert!(text.contains(r#""value":"500","count":4"#), "json 500: {}", text);
}

#[test]
fn aggregate_skips_shards_without_the_facet() {
    // One shard has the facet; the other is a plain archive. The
    // plain archive contributes zero (silently), the first shard's
    // counts are returned unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let a = build_shard(tmp.path(), "withfacet", 6, "a"); // 2× each
    fs::create_dir_all(tmp.path().join("plain/d")).unwrap();
    fs::write(tmp.path().join("plain/d/file.txt"), b"hello\n").unwrap();
    let plain = tmp.path().join("plain.zip");
    run(
        tmp.path(),
        &[
            "create",
            plain.file_name().unwrap().to_str().unwrap(),
            "plain/d",
            "-m",
            "deflate",
            "-q",
        ],
    );
    let out = run(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            a.to_str().unwrap(),
            plain.to_str().unwrap(),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("2  200"), "200 wrong: {}", text);
    assert!(text.contains("3 distinct value(s), 6 total entries across 2 shards"));
}

#[test]
fn aggregate_errors_when_no_shard_has_the_facet() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("d")).unwrap();
    fs::write(tmp.path().join("d/file.txt"), b"hello\n").unwrap();
    run(
        tmp.path(),
        &["create", "a.zip", "d", "-m", "deflate", "-q"],
    );
    run(
        tmp.path(),
        &["create", "b.zip", "d", "-m", "deflate", "-q"],
    );
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&["aggregate", "--by", "nonexistent", "a.zip", "b.zip"])
        .output()
        .expect("run tzip");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no facet") && err.contains("2 shard"),
        "unhelpful error: {}",
        err
    );
}

#[test]
fn query_unions_shards_via_duckdb() {
    if !have_duckdb() {
        eprintln!("skipping: duckdb not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let a = build_shard(tmp.path(), "a", 9, "a"); // 3× each of 200/404/500
    let b = build_shard(tmp.path(), "b", 9, "b");
    let out = run(
        tmp.path(),
        &[
            "query",
            "--format",
            "csv",
            "-s",
            "SELECT status_code, COUNT(*) AS n FROM tzip GROUP BY status_code ORDER BY status_code",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("200,6"), "200 UNION wrong: {}", text);
    assert!(text.contains("404,6"), "404 UNION wrong: {}", text);
    assert!(text.contains("500,6"), "500 UNION wrong: {}", text);
}

#[test]
fn query_reports_missing_sidecar_from_any_shard() {
    // One shard has columnar, the other doesn't.  Should bail with
    // a clear message pointing at the offending shard, not silently
    // return partial data.
    let tmp = tempfile::tempdir().unwrap();
    let good = build_shard(tmp.path(), "withcol", 3, "a");
    // Plain archive — no --write-columnar.
    fs::create_dir_all(tmp.path().join("d")).unwrap();
    fs::write(tmp.path().join("d/file.txt"), b"x\n").unwrap();
    let bad = tmp.path().join("plain.zip");
    run(
        tmp.path(),
        &[
            "create",
            bad.file_name().unwrap().to_str().unwrap(),
            "d",
            "-m",
            "deflate",
            "-q",
        ],
    );
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&[
            "query",
            "-s",
            "SELECT COUNT(*) FROM tzip",
            good.to_str().unwrap(),
            bad.to_str().unwrap(),
        ])
        .output()
        .expect("run tzip");
    assert!(!out.status.success(), "query should have failed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no columnar sidecar") && err.contains("plain.zip"),
        "error should name the shard missing the sidecar: {}",
        err
    );
}
