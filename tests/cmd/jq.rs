//! `tzip jq FILTER ARCHIVE...` integration tests — exercise the
//! streamed JSON filter via jaq. Confirms:
//!
//! - Basic field-access filter works on every JSON entry.
//! - `-r` matches jq's raw-output semantic (strings lose quotes).
//! - `--entry-prefix` tags each line with its source entry.
//! - Shard fan-out (multi-archive) works via the Phase 7 glob
//!   expander.
//! - `.tzip/*.v1` sentinel entries get skipped automatically.
//! - Non-JSON entries are tolerated (verbose warning, no abort).
//! - Rich filters (select, map, composition) compile + run.

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

fn make_corpus(root: &Path, dir: &str, items: &[(i64, &str)]) {
    let logs = root.join(dir).join("logs");
    fs::create_dir_all(&logs).unwrap();
    for (i, (status, user)) in items.iter().enumerate() {
        fs::write(
            logs.join(format!("log_{:04}.json", i)),
            format!(
                r#"{{"status_code":{},"user":"{}","path":"/api/{}"}}"#,
                status, user, user
            ),
        )
        .unwrap();
    }
}

#[test]
fn basic_field_access() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(
        tmp.path(),
        "a",
        &[(200, "alice"), (404, "bob"), (500, "carol")],
    );
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );
    let out = run(tmp.path(), &["jq", ".status_code", "out.zip"]);
    let text = String::from_utf8_lossy(&out.stdout);
    // Entry ordering isn't guaranteed; just check all three
    // numbers show up as individual lines.
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 lines, got {:?}", lines);
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["200", "404", "500"]);
}

#[test]
fn raw_output_strips_string_quotes() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", &[(200, "alice"), (200, "bob")]);
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );
    // Without -r: strings print as JSON (quoted).
    let out = run(tmp.path(), &["jq", ".user", "out.zip"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("\"alice\""),
        "default mode should keep quotes: {}",
        text
    );

    // With -r: strings print raw.
    let out = run(tmp.path(), &["jq", "-r", ".user", "out.zip"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.iter().any(|l| *l == "alice"),
        "raw output should include bare `alice`: {:?}",
        lines
    );
    assert!(
        !text.contains("\""),
        "raw output should have no quotes: {}",
        text
    );
}

#[test]
fn select_filter_narrows_rows() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(
        tmp.path(),
        "a",
        &[(200, "alice"), (404, "bob"), (500, "carol"), (404, "dave")],
    );
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );
    let out = run(
        tmp.path(),
        &["jq", "-r", "select(.status_code == 404) | .user", "out.zip"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, vec!["bob", "dave"]);
}

#[test]
fn entry_prefix_tags_each_line() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", &[(200, "alice"), (404, "bob")]);
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );
    let out = run(
        tmp.path(),
        &["jq", "--entry-prefix", "-r", ".user", "out.zip"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        assert!(
            line.starts_with("out.zip:a/logs/log_")
                && line.contains('\t'),
            "line should be prefixed with `archive:entry\\t`: {:?}",
            line
        );
    }
}

#[test]
fn shard_fan_out_merges_archives() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", &[(200, "alice"), (200, "bob")]);
    make_corpus(tmp.path(), "b", &[(500, "carol"), (500, "dave")]);
    run(
        tmp.path(),
        &["create", "a.zip", "a/logs", "-m", "deflate", "-q"],
    );
    run(
        tmp.path(),
        &["create", "b.zip", "b/logs", "-m", "deflate", "-q"],
    );
    let out = run(
        tmp.path(),
        &["jq", "-r", ".user", "a.zip", "b.zip"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, vec!["alice", "bob", "carol", "dave"]);
}

#[test]
fn skips_tzip_sentinel_entries() {
    // Archive carries a `.tzip/names.v1` sentinel but that's not
    // JSON — `tzip jq` should ignore it instead of erroring.
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", &[(200, "alice"), (404, "bob")]);
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "a/logs",
            "-m",
            "deflate",
            "--index-names",
            "-q",
        ],
    );
    let out = run(tmp.path(), &["jq", "-r", ".user", "out.zip"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, vec!["alice", "bob"], "stdout: {}", text);
}

#[test]
fn non_json_entries_are_tolerated() {
    let tmp = tempfile::tempdir().unwrap();
    let mix = tmp.path().join("mix");
    fs::create_dir_all(&mix).unwrap();
    fs::write(mix.join("a.json"), r#"{"user":"alice"}"#).unwrap();
    // Include a bogus-content `.json` file; the filter should
    // skip it (warning-only on --verbose, error-free otherwise)
    // and keep processing valid entries.
    fs::write(mix.join("b.json"), "this is not json").unwrap();
    fs::write(mix.join("c.json"), r#"{"user":"carol"}"#).unwrap();
    run(
        tmp.path(),
        &["create", "out.zip", "mix", "-m", "deflate", "-q"],
    );
    let out = run(tmp.path(), &["jq", "-r", ".user", "out.zip"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    // alice + carol survive; the bogus entry is silently dropped.
    assert_eq!(lines, vec!["alice", "carol"]);
}

#[test]
fn rich_filter_compiles_and_runs() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(
        tmp.path(),
        "a",
        &[(200, "alice"), (404, "bob"), (500, "carol")],
    );
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );
    // Build a struct via jq's object-construction syntax.
    let out = run(
        tmp.path(),
        &[
            "jq",
            r#"{status: .status_code, who: .user}"#,
            "out.zip",
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {:?} is not JSON: {}", line, e));
        assert!(v.get("status").is_some());
        assert!(v.get("who").is_some());
    }
}

#[test]
fn inner_jobs_parallel_matches_single_worker() {
    // Build a 60-entry archive and run the same filter under
    // --inner-jobs 1 and --inner-jobs 8. Set membership must be
    // identical — any serialisation seam or per-worker filter-state
    // bug would drop or duplicate lines.
    let tmp = tempfile::tempdir().unwrap();
    let items: Vec<(i64, &str)> = (0..60)
        .map(|i| (200 + (i % 3) as i64, if i % 2 == 0 { "alice" } else { "bob" }))
        .collect();
    make_corpus(tmp.path(), "a", &items);
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );

    let serial = run(
        tmp.path(),
        &["jq", "-r", ".user", "out.zip", "--inner-jobs", "1"],
    );
    let parallel = run(
        tmp.path(),
        &["jq", "-r", ".user", "out.zip", "--inner-jobs", "8"],
    );
    let mut s: Vec<String> = String::from_utf8_lossy(&serial.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    let mut p: Vec<String> = String::from_utf8_lossy(&parallel.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    s.sort();
    p.sort();
    assert_eq!(p, s, "parallel and serial jq must produce the same set");
    assert_eq!(p.len(), 60, "every entry must be filtered");
}

#[test]
fn bad_filter_errors_out_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), "a", &[(200, "alice")]);
    run(
        tmp.path(),
        &["create", "out.zip", "a/logs", "-m", "deflate", "-q"],
    );
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&["jq", "{ this is not valid jq", "out.zip"])
        .output()
        .expect("run tzip");
    assert!(!out.status.success(), "bad filter should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.to_lowercase().contains("jq"),
        "error should mention the jq filter: {}",
        err
    );
}
