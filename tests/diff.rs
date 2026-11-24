//! End-to-end `tzip diff` tests. Covers added/removed/changed
//! classification, --json output, --include/--exclude filtering,
//! and the shell exit codes (0 = identical, 1 = differed).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run_tzip_create(cwd: &Path, args: &[&str]) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run tzip create");
    assert!(
        out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_diff(cwd: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .arg("diff")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn tzip diff");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Write a tree of files under `<root>/<subdir>/c/<path>`. The `c`
/// prefix is intentional: `tzip create` uses the input root name as
/// the archive's entry prefix, so both sides of a diff need the same
/// name to make matching entries line up. Returns the path to pass to
/// `tzip create`.
fn seed_corpus(root: &Path, subdir: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let base = root.join(subdir);
    let dir = base.join("c");
    fs::create_dir_all(&dir).unwrap();
    for (path, body) in files {
        let p = dir.join(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, body).unwrap();
    }
    base
}

#[test]
fn diff_identical_archives_returns_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let a_root = seed_corpus(
        tmp.path(),
        "left",
        &[("a.txt", b"hello a\n"), ("b.txt", b"hello b\n")],
    );
    let b_root = seed_corpus(
        tmp.path(),
        "right",
        &[("a.txt", b"hello a\n"), ("b.txt", b"hello b\n")],
    );
    run_tzip_create(&a_root, &["create", "../x.zip", "c", "-q"]);
    run_tzip_create(&b_root, &["create", "../y.zip", "c", "-q"]);

    let (code, stdout, _) = run_diff(tmp.path(), &["x.zip", "y.zip"]);
    assert_eq!(code, Some(0), "expected exit 0 on identical archives");
    assert!(stdout.trim().is_empty(), "expected empty output: {stdout}");
}

#[test]
fn diff_adds_removes_and_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let a_root = seed_corpus(
        tmp.path(),
        "left",
        &[
            ("same.txt", b"unchanged\n"),
            ("will_change.txt", b"old body\n"),
            ("will_remove.txt", b"gone in b\n"),
        ],
    );
    let b_root = seed_corpus(
        tmp.path(),
        "right",
        &[
            ("same.txt", b"unchanged\n"),
            ("will_change.txt", b"new body shape\n"),
            ("newly_added.txt", b"only in b\n"),
        ],
    );
    run_tzip_create(&a_root, &["create", "../a.zip", "c", "-q"]);
    run_tzip_create(&b_root, &["create", "../b.zip", "c", "-q"]);

    let (code, stdout, _) = run_diff(tmp.path(), &["a.zip", "b.zip"]);
    assert_eq!(code, Some(1), "expected exit 1 on differ");
    assert!(stdout.contains("c/will_change.txt"), "missing changed: {stdout}");
    assert!(stdout.contains("c/will_remove.txt"), "missing removed: {stdout}");
    assert!(stdout.contains("c/newly_added.txt"), "missing added: {stdout}");
    assert!(!stdout.contains("c/same.txt"), "unchanged leaked: {stdout}");
}

#[test]
fn diff_names_only_format() {
    let tmp = tempfile::tempdir().unwrap();
    let a_root = seed_corpus(tmp.path(), "left", &[("one.txt", b"a one\n")]);
    let b_root = seed_corpus(tmp.path(), "right", &[("two.txt", b"b two\n")]);
    run_tzip_create(&a_root, &["create", "../a.zip", "c", "-q"]);
    run_tzip_create(&b_root, &["create", "../b.zip", "c", "-q"]);

    let (code, stdout, _) = run_diff(tmp.path(), &["a.zip", "b.zip", "--names-only"]);
    assert_eq!(code, Some(1));
    let mut lines: Vec<&str> = stdout.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        vec!["+ c/two.txt", "- c/one.txt"],
        "unexpected names-only output: {stdout}"
    );
}

#[test]
fn diff_json_output_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let a_root = seed_corpus(tmp.path(), "left", &[("f.txt", b"old\n")]);
    let b_root = seed_corpus(tmp.path(), "right", &[("f.txt", b"new body\n")]);
    run_tzip_create(&a_root, &["create", "../a.zip", "c", "-q"]);
    run_tzip_create(&b_root, &["create", "../b.zip", "c", "-q"]);

    let (code, stdout, _) = run_diff(tmp.path(), &["a.zip", "b.zip", "--json"]);
    assert_eq!(code, Some(1));
    let line = stdout.lines().next().expect("at least one record");
    let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
    assert_eq!(v["status"], "changed");
    assert_eq!(v["entry"].as_str().unwrap(), "c/f.txt");
    assert_eq!(v["size_a"].as_u64().unwrap(), b"old\n".len() as u64);
    assert_eq!(v["size_b"].as_u64().unwrap(), b"new body\n".len() as u64);
    assert!(v["crc32_a"].is_number());
    assert!(v["crc32_b"].is_number());
    assert_ne!(v["crc32_a"], v["crc32_b"]);
}

#[test]
fn diff_include_filter_suppresses_other_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let a_root = seed_corpus(
        tmp.path(),
        "left",
        &[("keep.log", b"kept\n"), ("noise.txt", b"ignored\n")],
    );
    let b_root = seed_corpus(
        tmp.path(),
        "right",
        &[
            ("keep.log", b"kept changed\n"),
            ("noise.txt", b"ignored too\n"),
        ],
    );
    run_tzip_create(&a_root, &["create", "../a.zip", "c", "-q"]);
    run_tzip_create(&b_root, &["create", "../b.zip", "c", "-q"]);

    // Only *.log — noise.txt difference shouldn't appear.
    let (code, stdout, _) = run_diff(
        tmp.path(),
        &["a.zip", "b.zip", "--include", "*.log", "--names-only"],
    );
    assert_eq!(code, Some(1));
    assert!(stdout.contains("keep.log"));
    assert!(!stdout.contains("noise.txt"), "exclude filter leaked: {stdout}");
}

#[test]
fn diff_mtime_only_suppressed_by_default_shown_with_flag() {
    // Produce two archives with the same content but shifted mtime.
    // Use `touch -d` on the input tree so tzip create picks up the
    // updated DOS time and the two archives differ only in that.
    let tmp = tempfile::tempdir().unwrap();
    let a_root = seed_corpus(tmp.path(), "left", &[("f.txt", b"identical content\n")]);
    run_tzip_create(&a_root, &["create", "../a.zip", "c", "-q"]);
    // Shift mtime on the source file and rebuild into b.zip from the
    // SAME source tree (so entry contents match byte-for-byte).
    let st = Command::new("touch")
        .args(["-d", "2000-01-01 00:00:00 UTC"])
        .arg(a_root.join("c/f.txt"))
        .status();
    if st.map(|s| !s.success()).unwrap_or(true) {
        eprintln!("skipping: touch -d not available");
        return;
    }
    run_tzip_create(&a_root, &["create", "../b.zip", "c", "-q"]);

    // Default: mtime-only diffs are suppressed → identical, exit 0.
    let (code, stdout, _) = run_diff(tmp.path(), &["a.zip", "b.zip"]);
    assert_eq!(code, Some(0));
    assert!(stdout.trim().is_empty());

    // With --mtime: the entry surfaces as T.
    let (code, stdout, _) = run_diff(tmp.path(), &["a.zip", "b.zip", "--mtime"]);
    assert_eq!(code, Some(1));
    assert!(stdout.contains("c/f.txt"));
    assert!(stdout.contains("mtime") || stdout.contains("T "));
}
