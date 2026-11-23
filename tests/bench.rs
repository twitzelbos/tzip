//! End-to-end smoke for `tzip bench`. Runs `--quick` so the test
//! finishes in under 10 seconds; verifies the table lands with the
//! expected sections and the JSON output parses.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(tzip_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn tzip");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn bench_quick_runs_and_prints_sections() {
    let (ok, stdout, stderr) = run(&["bench", "--quick"]);
    assert!(ok, "bench failed: {stderr}");
    // Table always has the three default sections.
    assert!(stdout.contains("=== create ==="), "missing create section:\n{stdout}");
    assert!(stdout.contains("=== extract ==="), "missing extract section:\n{stdout}");
    assert!(stdout.contains("=== list ==="), "missing list section:\n{stdout}");
    // tzip row must appear in each section regardless of which external
    // tools are on PATH.
    let tzip_rows = stdout.matches("tzip").count();
    assert!(
        tzip_rows >= 3,
        "expected a tzip row per section; got {tzip_rows} rows in:\n{stdout}"
    );
}

#[test]
fn bench_json_emits_parseable_lines() {
    let (ok, stdout, stderr) = run(&["bench", "--quick", "--json"]);
    assert!(ok, "bench --json failed: {stderr}");
    let mut saw_tzip = false;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("bench --json emitted non-JSON line {line:?}: {e}"));
        assert!(v["benchmark"].is_string());
        assert!(v["tool"].is_string());
        assert!(v["median_ms"].is_u64());
        assert!(v["throughput_mib_per_s"].is_number());
        if v["tool"].as_str().unwrap_or("").starts_with("tzip") {
            saw_tzip = true;
        }
    }
    assert!(saw_tzip, "expected at least one tzip row in JSON output");
}

#[test]
fn bench_only_filter_restricts_sections() {
    let (ok, stdout, _) = run(&["bench", "--quick", "--only", "list"]);
    assert!(ok);
    assert!(stdout.contains("=== list ==="));
    assert!(!stdout.contains("=== create ==="), "unexpected create section: {stdout}");
    assert!(!stdout.contains("=== extract ==="), "unexpected extract section: {stdout}");
}
