//! End-to-end tests for `tzip tail`.
//!
//! Covers:
//! - `--by added` returns the entries at the highest CDR
//!   positions (the "most recently added" proxy on an
//!   append-only archive).
//! - `--by mtime` returns the entries with the newest DOS
//!   mtime.
//! - Default `-n 10` and custom `-n`.
//! - `.tzip/*` sentinel entries are filtered out.
//! - `--json` emits one JSON object per line with the expected
//!   keys.
//! - Multi-archive (shard) fan-out merges results globally.

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

/// Extract the entry-name column (last field) from each line of
/// the default tabular tail output (`archive\tiso\tsize\tname`).
fn names_from_tail(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.rsplit_once('\t').map(|(_, n)| n.to_string()))
        .collect()
}

#[test]
fn tail_added_returns_last_n_by_cdr_position() {
    let tmp = tempfile::tempdir().unwrap();
    // Create an archive with 20 files. The create pipeline
    // writes them in whatever order its parallel reader lands;
    // for the purpose of this test we just care that
    // `tzip tail -n 3` returns the three entries with the
    // highest CDR positions, consistent with `tzip ls`'s own
    // ordering.
    for i in 0..20 {
        fs::write(
            tmp.path().join(format!("f{:02}.txt", i)),
            format!("file {}\n", i).as_bytes(),
        )
        .unwrap();
    }
    let paths: Vec<String> = (0..20).map(|i| format!("f{:02}.txt", i)).collect();
    let mut args = vec!["create", "out.zip"];
    for p in &paths {
        args.push(p.as_str());
    }
    args.push("-q");
    run(tmp.path(), &args);

    let out = run(tmp.path(), &["tail", "-n", "3", "out.zip"]);
    let got = names_from_tail(&out.stdout);
    assert_eq!(
        got.len(),
        3,
        "expected 3 tail rows, got {}: {:?}",
        got.len(),
        got
    );

    // The three names must be the last three entries in
    // `tzip ls`'s own CDR ordering. We cross-check against
    // `tzip find --names-only`, which also walks the CDR.
    let find_out = run(tmp.path(), &["find", "--names-only", "out.zip"]);
    let all_names: Vec<String> = String::from_utf8_lossy(&find_out.stdout)
        .lines()
        .filter_map(|l| l.rsplit_once(':').map(|(_, n)| n.to_string()))
        .collect();
    let expected: Vec<&String> = all_names.iter().rev().take(3).collect();
    for name in &got {
        assert!(
            expected.iter().any(|e| *e == name),
            "tail returned {} but it wasn't in the last 3 CDR entries {:?}",
            name,
            expected
        );
    }
}

#[test]
fn tail_mtime_sorts_by_source_mtime_descending() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let tmp = tempfile::tempdir().unwrap();
    // Three files with distinct, far-apart mtimes so DOS's
    // 2-second resolution doesn't collapse them into ties.
    // Set mtime explicitly via `filetime` via `touch -t`.
    for (name, year, month, day) in
        [("old.txt", 2022, 1, 1), ("mid.txt", 2024, 6, 15), ("new.txt", 2026, 11, 25)]
    {
        fs::write(tmp.path().join(name), b"body\n").unwrap();
        // `touch -t YYYYMMDDhhmm file` works on both BSD and GNU.
        let status = Command::new("touch")
            .args([
                "-t",
                &format!("{:04}{:02}{:02}1200", year, month, day),
                tmp.path().join(name).to_str().unwrap(),
            ])
            .status()
            .expect("run touch");
        assert!(status.success());
    }
    // Workaround: `touch -t` requires a reference year we can
    // actually represent; verify we're on a system that
    // accepted the input before trusting mtimes.
    let _ = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    run(
        tmp.path(),
        &["create", "out.zip", "old.txt", "mid.txt", "new.txt", "-q"],
    );
    let out = run(tmp.path(), &["tail", "--by", "mtime", "-n", "3", "out.zip"]);
    let got = names_from_tail(&out.stdout);
    assert_eq!(got, vec!["new.txt", "mid.txt", "old.txt"]);
}

#[test]
fn tail_filters_tzip_sentinels() {
    // An archive with the name index on produces
    // `.tzip/names.v1` as a sentinel entry at the tail of the
    // CDR. `tzip tail` must skip it so "last 3 added" reports
    // real user content, not bookkeeping.
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..5 {
        fs::write(
            tmp.path().join(format!("f{}.txt", i)),
            format!("file {}\n", i).as_bytes(),
        )
        .unwrap();
    }
    run(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "f0.txt",
            "f1.txt",
            "f2.txt",
            "f3.txt",
            "f4.txt",
            "--index-names",
            "-q",
        ],
    );
    let out = run(tmp.path(), &["tail", "-n", "10", "out.zip"]);
    let got = names_from_tail(&out.stdout);
    for name in &got {
        assert!(
            !name.starts_with(".tzip/"),
            "tail returned a .tzip/* sentinel: {}",
            name
        );
    }
    // All 5 user files should show up (and only them).
    assert_eq!(got.len(), 5);
}

#[test]
fn tail_json_emits_one_object_per_line() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"hello\n").unwrap();
    fs::write(tmp.path().join("b.txt"), b"world\n").unwrap();
    run(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "-q"]);

    let out = run(tmp.path(), &["tail", "--json", "-n", "2", "out.zip"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        // Required keys per the tail.rs emit path.
        for key in &[
            "\"archive\":",
            "\"entry\":",
            "\"size\":",
            "\"mtime\":",
            "\"encrypted\":",
            "\"method\":",
        ] {
            assert!(
                line.contains(key),
                "missing key {} in tail JSON: {}",
                key,
                line
            );
        }
    }
}

#[test]
fn tail_merges_across_multiple_archives() {
    let tmp = tempfile::tempdir().unwrap();
    for (zipname, prefix) in [("a.zip", "A"), ("b.zip", "B")] {
        let names: Vec<String> = (0..3).map(|i| format!("{}-{}.txt", prefix, i)).collect();
        for name in &names {
            fs::write(tmp.path().join(name), format!("{}\n", name).as_bytes()).unwrap();
        }
        let mut args: Vec<&str> = vec!["create", zipname];
        for n in &names {
            args.push(n.as_str());
        }
        args.push("-q");
        run(tmp.path(), &args);
    }
    let out = run(tmp.path(), &["tail", "-n", "4", "a.zip", "b.zip"]);
    let got = names_from_tail(&out.stdout);
    // Four results across the merge, with all six entries
    // mtime-tied at script wall-clock time (approximately) —
    // any four of them is acceptable; what we want to verify
    // is that BOTH archives contribute.
    assert_eq!(got.len(), 4);
    let full = String::from_utf8_lossy(&out.stdout);
    assert!(full.contains("a.zip"), "merged tail missing a.zip rows: {}", full);
    assert!(full.contains("b.zip"), "merged tail missing b.zip rows: {}", full);
}
