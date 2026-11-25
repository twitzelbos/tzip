//! End-to-end tests for Phase 4 appendable index segments. Covers
//! `tzip u --index-names / --index-terms / --facet` writing a new
//! `.segN` entry for each index kind and the reader walking every
//! segment at query time.

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

/// Write `n` JSON log entries with statuses cycling across `[200, 404, 500]`.
fn make_json_corpus(root: &Path, n: usize) {
    fs::create_dir_all(root.join("logs")).unwrap();
    for i in 0..n {
        let status = match i % 3 {
            0 => 200,
            1 => 404,
            _ => 500,
        };
        let body = format!(
            "{{\"ts\":\"2026-11-24\",\"status_code\":{},\"user\":\"u{}\"}}\n",
            status,
            i % 5
        );
        fs::write(root.join(format!("logs/log_{:04}.json", i)), body).unwrap();
    }
}

#[test]
fn append_writes_name_segment_and_lookup_sees_both() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 10);
    run(
        tmp.path(),
        &["create", "out.zip", "logs", "-m", "deflate", "--index-names", "-q"],
    );
    // Add 3 more entries under a separate directory.
    fs::create_dir_all(tmp.path().join("more")).unwrap();
    fs::write(tmp.path().join("more/new_a.json"), br#"{"status_code":999}"#).unwrap();
    fs::write(tmp.path().join("more/new_b.json"), br#"{"status_code":999}"#).unwrap();
    fs::write(tmp.path().join("more/new_c.json"), br#"{"status_code":999}"#).unwrap();
    run(
        tmp.path(),
        &["u", "out.zip", "more", "--index-names", "-q"],
    );
    let listing = run(tmp.path(), &["ls", "out.zip"]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains(".tzip/names.v1"), "base name index missing");
    assert!(text.contains(".tzip/names.v1.seg1"), "append name segment missing");

    // Old entry still resolvable (via base segment).
    let got = run(tmp.path(), &["cat", "out.zip", "logs/log_0005.json"]);
    assert!(!got.stdout.is_empty());

    // New entry resolvable (must be via seg1 — base doesn't know it).
    let got = run(tmp.path(), &["cat", "out.zip", "more/new_b.json"]);
    assert_eq!(
        String::from_utf8_lossy(&got.stdout),
        "{\"status_code\":999}"
    );
}

#[test]
fn append_writes_facet_segment_and_aggregate_merges() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 9); // 3× each of 200, 404, 500
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
    fs::create_dir_all(tmp.path().join("more")).unwrap();
    fs::write(tmp.path().join("more/new_1.json"), br#"{"status_code":200}"#).unwrap();
    fs::write(tmp.path().join("more/new_2.json"), br#"{"status_code":999}"#).unwrap();
    run(
        tmp.path(),
        &[
            "u",
            "out.zip",
            "more",
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
    assert!(text.contains(".tzip/facets.v1/status_code.seg1"));

    let agg = run(tmp.path(), &["aggregate", "--by", "status_code", "out.zip"]);
    let a = String::from_utf8_lossy(&agg.stdout);
    // After append: 4 × 200, 3 × 404, 3 × 500, 1 × 999 = 11 total,
    // 4 distinct.
    assert!(a.contains("           4  200"), "status 200 count merged wrong: {}", a);
    assert!(a.contains("           3  404"), "status 404 count wrong: {}", a);
    assert!(a.contains("           3  500"), "status 500 count wrong: {}", a);
    assert!(a.contains("           1  999"), "appended status 999 missing: {}", a);
    assert!(a.contains("4 distinct value(s), 11 total"));
}

#[test]
fn append_writes_term_segment_and_grep_sees_new_entries() {
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 5);
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
            "-q",
        ],
    );
    fs::create_dir_all(tmp.path().join("more")).unwrap();
    fs::write(
        tmp.path().join("more/incident.json"),
        br#"{"error":"OutOfMemoryError in worker 42"}"#,
    )
    .unwrap();
    run(
        tmp.path(),
        &["u", "out.zip", "more", "--index-terms", "*.json", "-q"],
    );

    // `tzip grep` with the literal term should find the newly
    // appended entry via the `.tzip/terms.v1.seg1` segment.
    let out = run(tmp.path(), &["grep", "OutOfMemoryError", "out.zip"]);
    let hits = String::from_utf8_lossy(&out.stdout);
    assert!(
        hits.contains("more/incident.json"),
        "grep didn't find appended entry: {}",
        hits
    );
}

#[test]
fn segments_sum_counts_across_multiple_appends() {
    // Verify that TWO append operations each write their own
    // `.segN` and the aggregate command correctly merges all three
    // (base + seg1 + seg2).
    let tmp = tempfile::tempdir().unwrap();
    make_json_corpus(tmp.path(), 3); // 1× each of 200, 404, 500
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
    // First append.
    fs::create_dir_all(tmp.path().join("pass_a")).unwrap();
    fs::write(tmp.path().join("pass_a/x.json"), br#"{"status_code":200}"#).unwrap();
    run(
        tmp.path(),
        &[
            "u",
            "out.zip",
            "pass_a",
            "--index-terms",
            "*.json",
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    // Second append.
    fs::create_dir_all(tmp.path().join("pass_b")).unwrap();
    fs::write(tmp.path().join("pass_b/y.json"), br#"{"status_code":200}"#).unwrap();
    run(
        tmp.path(),
        &[
            "u",
            "out.zip",
            "pass_b",
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
    assert!(text.contains(".tzip/facets.v1/status_code.seg1"));
    assert!(text.contains(".tzip/facets.v1/status_code.seg2"));

    let agg = run(tmp.path(), &["aggregate", "--by", "status_code", "out.zip"]);
    let a = String::from_utf8_lossy(&agg.stdout);
    // Total: 3 original + 2 appended = 5 entries, 3 × 200 + 1 × 404 + 1 × 500.
    assert!(a.contains("           3  200"), "merged count for 200: {}", a);
    assert!(a.contains("5 total"), "total wrong: {}", a);
}
