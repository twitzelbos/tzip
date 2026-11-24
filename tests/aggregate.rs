//! End-to-end tests for `tzip create --facet` + `tzip aggregate`.
//! The facet payload is a plain `.tzip/facets.v1/<leaf>` STORE entry,
//! so the aggregate path reads it directly without any decompression.

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

fn make_json_corpus(root: &Path, n: usize) {
    fs::create_dir_all(root.join("logs")).unwrap();
    for i in 0..n {
        // Cycle through 4 status codes + 7 users for a non-trivial
        // distribution.
        let status = match i % 4 {
            0 => 200,
            1 => 404,
            2 => 500,
            _ => 503,
        };
        let user = i % 7;
        let body = format!(
            "{{\"ts\":\"2026-11-24\",\"status_code\":{},\"user\":\"u{}\"}}\n",
            status, user
        );
        fs::write(root.join(format!("logs/log_{:04}.json", i)), body).unwrap();
    }
}

#[test]
fn facet_entry_written_and_listed() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 20);
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
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains(".tzip/facets.v1/status_code"));

    let fields = run(tmp.path(), &["aggregate", "--fields", "out.zip"]);
    let names = String::from_utf8_lossy(&fields.stdout);
    assert!(names.lines().any(|l| l == "status_code"));
}

#[test]
fn aggregate_count_by_field() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 100);
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
            "--facet",
            ".status_code",
            "--facet",
            ".user",
            "-q",
        ],
    );
    let agg = run(tmp.path(), &["aggregate", "--by", "status_code", "out.zip"]);
    let text = String::from_utf8_lossy(&agg.stdout);
    // Each of the 4 statuses gets 25 entries (100 % 4 == 0).
    for status in ["200", "404", "500", "503"] {
        assert!(
            text.contains(&format!("          25  {}", status)),
            "missing {}: {}",
            status,
            text
        );
    }
}

#[test]
fn aggregate_top_n_limits_output() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 70);
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
            "--facet",
            ".user",
            "-q",
        ],
    );
    let agg = run(tmp.path(), &["aggregate", "--by", "user", "--top", "3", "out.zip"]);
    let text = String::from_utf8_lossy(&agg.stdout);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .collect();
    assert_eq!(lines.len(), 3, "expected 3 lines, got: {}", text);
}

#[test]
fn aggregate_json_format() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 20);
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
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    let agg = run(tmp.path(), &["aggregate", "--by", "status_code", "--json", "out.zip"]);
    let text = String::from_utf8_lossy(&agg.stdout);
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        assert!(l.starts_with('{') && l.ends_with('}'), "bad JSON line: {}", l);
        assert!(l.contains("\"value\":"));
        assert!(l.contains("\"count\":"));
    }
}

#[test]
fn aggregate_without_facet_errors_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 10);
    run(tmp.path(), &["create", "out.zip", "logs", "-m", "deflate", "-q"]);
    let bogus = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&["aggregate", "--by", "status_code", "out.zip"])
        .output()
        .unwrap();
    assert!(!bogus.status.success(), "aggregate on an un-faceted archive must fail cleanly");
    let err = String::from_utf8_lossy(&bogus.stderr);
    assert!(err.contains("no facet"), "unhelpful error: {}", err);
}

#[test]
fn facet_handles_missing_field_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("logs")).unwrap();
    // Some entries have status_code, some don't.
    fs::write(
        tmp.path().join("logs/a.json"),
        br#"{"status_code":200}"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("logs/b.json"),
        br#"{"status_code":500}"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("logs/c.json"),
        br#"{"other":"x"}"#,
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
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    let agg = run(tmp.path(), &["aggregate", "--by", "status_code", "out.zip"]);
    let text = String::from_utf8_lossy(&agg.stdout);
    assert!(text.contains("__missing__"), "no __missing__ bucket: {}", text);
}
