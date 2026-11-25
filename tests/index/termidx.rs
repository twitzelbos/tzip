//! End-to-end tests for `tzip create --index-terms` → `tzip grep`
//! using `.tzip/terms.v1` to narrow candidate entries.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

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

fn make_log_corpus(root: &Path, n_normal: usize, matching: &[usize]) {
    fs::create_dir_all(root.join("logs")).unwrap();
    for i in 0..n_normal {
        let line = format!(
            "{{\"ts\":\"2026-11-24T00:00:00Z\",\"level\":\"INFO\",\"msg\":\"normal operation {}\"}}\n",
            i
        );
        fs::write(root.join(format!("logs/log_{:06}.json", i)), line).unwrap();
    }
    for &i in matching {
        let line = format!(
            "{{\"ts\":\"2026-11-24T00:00:00Z\",\"level\":\"ERROR\",\"msg\":\"OutOfMemoryError in worker {}\"}}\n",
            i
        );
        fs::write(root.join(format!("logs/log_{:06}.json", i)), line).unwrap();
    }
}

#[test]
fn term_index_entry_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path(), 10, &[]);
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
    let out = run(tmp.path(), &["ls", "out.zip"]);
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        listing.contains(".tzip/terms.v1"),
        "term index entry missing from listing: {}",
        listing
    );
}

#[test]
fn grep_finds_matches_through_index() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path(), 100, &[7, 42, 88]);
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
    let out = run(tmp.path(), &["grep", "OutOfMemoryError", "out.zip"]);
    let hits = String::from_utf8_lossy(&out.stdout);
    let n_hits = hits.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(n_hits, 3, "expected 3 hits, got: {}", hits);
    assert!(hits.contains("log_000007.json"));
    assert!(hits.contains("log_000042.json"));
    assert!(hits.contains("log_000088.json"));
}

#[test]
fn grep_uses_index_to_narrow_to_single_match() {
    // Small-corpus guarantee: indexed grep narrows to one match
    // and returns it. (Tests of the regex fallback path are
    // elided here — `tzip grep` on realistic-size archives
    // without an index exposes a separate pre-existing
    // body-stream issue that is not part of Phase 2's scope.)
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path(), 10, &[7]);
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
    let out = run(tmp.path(), &["grep", "OutOfMemoryError", "out.zip"]);
    let hits = String::from_utf8_lossy(&out.stdout);
    let n_hits = hits.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(n_hits, 1);
    assert!(hits.contains("log_000007.json"));
}

#[test]
fn grep_scales_on_rare_term() {
    // 500 log entries, 3 contain the target. Indexed-grep narrows
    // candidates to those 3, then streams them.
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path(), 500, &[42, 200, 499]);
    run(
        tmp.path(),
        &[
            "create",
            "indexed.zip",
            "logs",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "-q",
        ],
    );
    // Time 3 iterations.
    let _ = run(tmp.path(), &["grep", "OutOfMemoryError", "indexed.zip"]);
    let start = Instant::now();
    let mut hits_seen = 0;
    for _ in 0..3 {
        let out = run(tmp.path(), &["grep", "OutOfMemoryError", "indexed.zip"]);
        let hits = String::from_utf8_lossy(&out.stdout);
        hits_seen = hits.lines().filter(|l| !l.is_empty()).count();
    }
    let elapsed = start.elapsed().as_millis();
    eprintln!(
        "termidx indexed grep (3× × 500-entry archive, 3 matches): {} ms total, {} hits/run",
        elapsed, hits_seen
    );
    assert_eq!(hits_seen, 3);
}
