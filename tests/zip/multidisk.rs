//! End-to-end tests for multi-disk (spanned) ZIP archives. All
//! fixtures come from Info-ZIP's `zip -s <size>` — the de facto
//! reference encoder for the format. Tests are gated on
//! `have_zip_cmd()` so CI environments without Info-ZIP skip cleanly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run_tzip(cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run tzip");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn have_zip_cmd() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("command -v zip >/dev/null 2>&1")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a split archive. Returns the number of .z0N files produced
/// (so the test can sanity-check the split actually happened).
fn make_split_archive(cwd: &Path, out: &str, split_size: &str, inputs: &[&str]) -> usize {
    // `-0` (store) so our deterministic `pattern_bytes` data — which is
    // highly compressible — still fills enough bytes to force multi-
    // disk output. The reader doesn't care whether entries are stored
    // or deflated; what we're exercising is disk-span routing.
    let mut args = vec!["-q", "-0", "-s", split_size, out];
    args.extend_from_slice(inputs);
    let st = Command::new("zip")
        .current_dir(cwd)
        .args(&args)
        .status()
        .expect("spawn zip -s");
    assert!(st.success(), "zip -s failed");
    // Count the .z0N siblings alongside `out`.
    fs::read_dir(cwd)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().into_string().unwrap_or_default();
            n.starts_with(out.trim_end_matches(".zip")) && n.contains(".z0")
        })
        .count()
}

/// Deterministic byte pattern — byte = (i * 7) & 0xff.
fn pattern_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(7) & 0xff) as u8).collect()
}

#[test]
fn ls_lists_entries_across_disks() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // 20 small files, ~10 KiB each, split at 64 KiB → several disks.
    for i in 1..=20 {
        fs::write(
            tmp.path().join(format!("f{i:02}.bin")),
            pattern_bytes(10 * 1024),
        )
        .unwrap();
    }
    let inputs: Vec<String> = (1..=20).map(|i| format!("f{i:02}.bin")).collect();
    let input_refs: Vec<&str> = inputs.iter().map(|s| s.as_str()).collect();
    let disk_count = make_split_archive(tmp.path(), "big.zip", "64k", &input_refs);
    assert!(disk_count >= 2, "expected split into multiple disks, got {disk_count}");

    let (ok, stdout, stderr) = run_tzip(tmp.path(), &["ls", "big.zip"]);
    assert!(ok, "ls failed: {stderr}");
    for i in 1..=20 {
        let name = format!("f{i:02}.bin");
        assert!(stdout.contains(&name), "ls missing entry {name}:\n{stdout}");
    }
    assert!(stdout.contains("20 files"));
}

#[test]
fn cat_reads_entry_that_spans_disk_boundary() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // One 200 KiB entry split across ~64 KiB disks → the body itself
    // crosses disk boundaries. (Info-ZIP splits at arbitrary byte
    // offsets within compressed data; the reader must handle this.)
    let payload = pattern_bytes(200 * 1024);
    fs::write(tmp.path().join("big.bin"), &payload).unwrap();
    let disk_count = make_split_archive(tmp.path(), "big.zip", "64k", &["big.bin"]);
    assert!(disk_count >= 2, "expected multi-disk, got {disk_count}");

    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(["cat", "big.zip", "big.bin"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "cat failed");
    assert_eq!(out.stdout, payload);
}

#[test]
fn extract_restores_all_entries_from_split_archive() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let names = ["a.bin", "b.bin", "c.bin", "d.bin"];
    // Info-ZIP's minimum split size is 64 KiB. ~100 KiB per file × 4
    // files = 400 KiB stored → ~6 disks at 64k.
    let bodies: Vec<Vec<u8>> = (0..names.len())
        .map(|i| pattern_bytes(100 * 1024 + i * 7))
        .collect();
    for (name, body) in names.iter().zip(&bodies) {
        fs::write(tmp.path().join(name), body).unwrap();
    }
    let disk_count = make_split_archive(tmp.path(), "big.zip", "64k", &names);
    assert!(disk_count >= 3, "expected several disks, got {disk_count}");

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["x", "big.zip", "--to", out_dir.to_str().unwrap()],
    );
    assert!(ok, "extract failed: {stderr}");
    for (name, body) in names.iter().zip(&bodies) {
        let got = fs::read(out_dir.join(name)).unwrap();
        assert_eq!(&got, body, "mismatch on {name}");
    }
}

#[test]
fn find_lists_entries_from_split_archive() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for i in 1..=5 {
        fs::write(
            tmp.path().join(format!("file{i}.txt")),
            pattern_bytes(60 * 1024),
        )
        .unwrap();
    }
    let inputs: Vec<String> = (1..=5).map(|i| format!("file{i}.txt")).collect();
    let input_refs: Vec<&str> = inputs.iter().map(|s| s.as_str()).collect();
    // ~300 KiB total → ~4 disks at the 64 KiB minimum split size.
    let disk_count = make_split_archive(tmp.path(), "big.zip", "64k", &input_refs);
    assert!(disk_count >= 2, "expected split, got {disk_count}");

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "big.zip", "--names-only"],
    );
    assert!(ok);
    assert_eq!(
        stdout.lines().count(),
        5,
        "expected 5 entries in find output:\n{stdout}"
    );
}

#[test]
fn missing_sibling_disk_fails_with_clear_error() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("payload.bin"), pattern_bytes(200 * 1024)).unwrap();
    make_split_archive(tmp.path(), "big.zip", "64k", &["payload.bin"]);
    // Nuke the first disk.
    fs::remove_file(tmp.path().join("big.z01")).unwrap();

    let (ok, _stdout, stderr) = run_tzip(tmp.path(), &["ls", "big.zip"]);
    assert!(!ok, "ls should fail with missing disk");
    assert!(
        stderr.contains("multi-disk") || stderr.contains("open"),
        "unhelpful error: {stderr}"
    );
}
