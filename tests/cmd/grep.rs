//! End-to-end `tzip grep` tests: local archives for the matcher /
//! filter / checkpoint / exec paths; `tiny_http` fixtures for HTTP and
//! S3-mock parallel fan-out and bucket-globbing.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

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

fn run_tzip(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
    let mut cmd = Command::new(tzip_bin());
    cmd.current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run tzip");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn write_log(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

// ── Local-archive tests ────────────────────────────────────────────────────

fn make_log_corpus(tmp: &Path) {
    fs::create_dir_all(tmp.join("corpus")).unwrap();
    write_log(
        &tmp.join("corpus/a.log"),
        "INFO startup\nERROR bad\nINFO ok\nERROR worse\n",
    );
    write_log(
        &tmp.join("corpus/b.log"),
        "INFO fine\nINFO also fine\n",
    );
    write_log(&tmp.join("corpus/notes.txt"), "no matching keywords here\n");
}

#[test]
fn grep_single_archive_basic() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, stderr) = run_tzip(tmp.path(), &["grep", "ERROR", "out.zip"], &[]);
    assert!(ok, "grep failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected match count: {stdout}");
    assert!(lines.iter().all(|l| l.contains("corpus/a.log")));
    assert!(lines.iter().any(|l| l.contains("ERROR bad")));
    assert!(lines.iter().any(|l| l.contains("ERROR worse")));
}

#[test]
fn grep_many_archives_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    for name in &["a", "b", "c"] {
        let d = tmp.path().join(name);
        fs::create_dir_all(&d).unwrap();
        write_log(
            &d.join("log.txt"),
            &format!("INFO start {n}\nERROR in {n}\nINFO end {n}\n", n = name),
        );
        run_tzip_create(
            tmp.path(),
            &["create", &format!("{}.zip", name), name, "-q"],
        );
    }
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", "a.zip", "b.zip", "c.zip", "-P", "3"],
        &[],
    );
    assert!(ok, "grep failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 matches, got: {stdout}");
    let archives: HashSet<String> = lines
        .iter()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    assert_eq!(archives, HashSet::from(["a.zip".into(), "b.zip".into(), "c.zip".into()]));
}

#[test]
fn grep_include_filter_skips_entries() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // `notes.txt` contains "no" which would match the regex `.` otherwise;
    // with --include '*.log', it should be skipped entirely.
    let (ok, _stdout, _stderr) = run_tzip(
        tmp.path(),
        &[
            "grep",
            ".",
            "out.zip",
            "--include",
            "*.log",
            "--files-only",
            "--checkpoint",
            "ck.jsonl",
        ],
        &[],
    );
    assert!(ok);
    let ck = fs::read_to_string(tmp.path().join("ck.jsonl")).unwrap();
    // Checkpoint should list the two .log entries but NOT notes.txt.
    assert!(ck.contains("corpus/a.log"));
    assert!(ck.contains("corpus/b.log"));
    assert!(!ck.contains("corpus/notes.txt"), "--include leaked: {ck}");
}

#[test]
fn grep_files_only() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", "out.zip", "--files-only"],
        &[],
    );
    assert!(ok);
    assert_eq!(stdout.trim(), "out.zip:corpus/a.log");
}

#[test]
fn grep_ignore_case() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(tmp.path(), &["grep", "-i", "error", "out.zip"], &[]);
    assert!(ok);
    assert!(stdout.contains("ERROR bad"));
    assert!(stdout.contains("ERROR worse"));
}

#[test]
fn grep_json_output() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", "out.zip", "--json"],
        &[],
    );
    assert!(ok);
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert!(v["archive"].is_string());
        assert!(v["entry"].is_string());
        assert!(v["line"].is_u64());
        assert!(v["match"].is_string());
    }
}

#[test]
fn grep_exec_fires_only_on_matched_entries() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // `wc -c` on the exec tempfile; verify we only see output for a.log
    // (which has ERROR matches), not b.log or notes.txt.
    let (ok, stdout, _stderr) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", "out.zip", "--exec", "wc -c {}"],
        &[],
    );
    assert!(ok);
    // One separator line + one wc output line.
    let separators: Vec<&str> = stdout.lines().filter(|l| l.starts_with(":::")).collect();
    assert_eq!(separators.len(), 1, "expected exactly one exec invocation; got:\n{stdout}");
    assert!(separators[0].contains("corpus/a.log"));
}

#[test]
fn grep_checkpoint_resume_skips_prior_entries() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // First run — produces matches + checkpoint.
    let (_ok, first, _) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", "out.zip", "--checkpoint", "ck.jsonl"],
        &[],
    );
    let first_matches: Vec<&str> = first.lines().collect();
    assert_eq!(first_matches.len(), 2);

    // Second run with --resume — all entries already in checkpoint, so
    // zero new output.
    let (ok, second, stderr) = run_tzip(
        tmp.path(),
        &[
            "grep",
            "ERROR",
            "out.zip",
            "--checkpoint",
            "ck.jsonl",
            "--resume",
        ],
        &[],
    );
    assert!(ok, "resume failed: {stderr}");
    assert!(second.trim().is_empty(), "resume re-emitted: {second}");
}

#[test]
fn grep_checkpoint_refuses_mismatched_query() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    run_tzip(
        tmp.path(),
        &["grep", "ERROR", "out.zip", "--checkpoint", "ck.jsonl"],
        &[],
    );
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &[
            "grep",
            "INFO",
            "out.zip",
            "--checkpoint",
            "ck.jsonl",
            "--resume",
        ],
        &[],
    );
    assert!(!ok, "resume should refuse mismatched query");
    assert!(stderr.contains("different query"), "stderr: {stderr}");
}

#[test]
fn grep_size_filter() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("corpus")).unwrap();
    fs::write(tmp.path().join("corpus/tiny.log"), "tiny ERROR line\n").unwrap();
    // 2 KB of ERROR + padding.
    let big = format!("ERROR big one\n{}\n", "padding ".repeat(500));
    fs::write(tmp.path().join("corpus/big.log"), big.as_bytes()).unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // Skip anything under 1 KiB → only big.log scanned.
    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &[
            "grep",
            "ERROR",
            "out.zip",
            "--min-size",
            "1024",
            "--files-only",
        ],
        &[],
    );
    assert!(ok);
    assert_eq!(stdout.trim(), "out.zip:corpus/big.log");
}

#[test]
fn grep_no_match_is_clean_exit() {
    let tmp = tempfile::tempdir().unwrap();
    make_log_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) =
        run_tzip(tmp.path(), &["grep", "NO_SUCH_PATTERN_EVER", "out.zip"], &[]);
    assert!(ok);
    assert!(stdout.is_empty());
}

#[test]
fn grep_entry_without_trailing_newline() {
    // Streaming scanner must still match the final line even when the
    // entry body doesn't end in `\n` — without materialising the body.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("corpus")).unwrap();
    fs::write(
        tmp.path().join("corpus/trailing.log"),
        // No trailing newline: last line is "ERROR final".
        b"first\nsecond\nERROR final",
    )
    .unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, stderr) = run_tzip(tmp.path(), &["grep", "ERROR", "out.zip"], &[]);
    assert!(ok, "grep failed: {stderr}");
    assert_eq!(
        stdout.lines().count(),
        1,
        "expected exactly one match, got:\n{stdout}"
    );
    let line = stdout.lines().next().unwrap();
    assert!(line.contains("corpus/trailing.log"));
    assert!(line.contains(":3:"), "line number should be 3: {line}");
    assert!(line.ends_with("ERROR final"));
}

#[test]
fn grep_streaming_match_spans_chunk_boundary() {
    // The scanner's line is `buf + chunk_slice` — the scan has to work
    // when the matching text straddles two chunks. A ~200 KiB entry with
    // the pattern deep inside a very long line exercises both the chunk
    // boundary and the read-across-\n behaviour.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("corpus")).unwrap();
    let mut body: Vec<u8> = Vec::with_capacity(220 * 1024);
    for i in 0..5000 {
        use std::io::Write as _;
        writeln!(body, "line {i} with some filler").unwrap();
    }
    // Inject the pattern inside one long line (no newline for a while).
    let mut long_line: Vec<u8> = b"START".to_vec();
    long_line.extend(std::iter::repeat(b'a').take(120 * 1024));
    long_line.extend_from_slice(b"NEEDLE");
    long_line.extend(std::iter::repeat(b'a').take(120 * 1024));
    long_line.push(b'\n');
    body.extend_from_slice(&long_line);
    body.extend_from_slice(b"tail\n");
    fs::write(tmp.path().join("corpus/wide.log"), &body).unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, stderr) =
        run_tzip(tmp.path(), &["grep", "NEEDLE", "out.zip", "--files-only"], &[]);
    assert!(ok, "chunk-boundary grep failed: {stderr}");
    assert_eq!(stdout.trim(), "out.zip:corpus/wide.log");
}

#[test]
fn grep_streaming_large_entry_memory_bounded() {
    // A 20 MiB entry of short lines, searched for a pattern that lives
    // in a known late line. With the old materialising scanner we'd
    // hold the full 20 MiB plaintext; here we only hold the trailing
    // partial line (< 1 KiB). This is a correctness test — the memory
    // claim is enforced structurally by LineScanner's buf semantics —
    // but a wrong implementation would fail on content.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("corpus")).unwrap();
    let mut body: Vec<u8> = Vec::with_capacity(20 * 1024 * 1024);
    let before = 200_000usize;
    for i in 0..before {
        use std::io::Write as _;
        writeln!(body, "normal line {i}").unwrap();
    }
    body.extend_from_slice(b"TARGETED_MATCH right here\n");
    for i in 0..before {
        use std::io::Write as _;
        writeln!(body, "more line {i}").unwrap();
    }
    fs::write(tmp.path().join("corpus/big.log"), &body).unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, stderr) =
        run_tzip(tmp.path(), &["grep", "TARGETED_MATCH", "out.zip"], &[]);
    assert!(ok, "big-entry grep failed: {stderr}");
    assert_eq!(stdout.lines().count(), 1);
    let expected_line_no = before as u64 + 1;
    assert!(
        stdout.contains(&format!(":{}:", expected_line_no)),
        "expected line number {expected_line_no} in: {stdout}"
    );
}

#[test]
fn grep_aes_entries_with_password() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("corpus")).unwrap();
    fs::write(
        tmp.path().join("corpus/secret.log"),
        "normal line\nSECRET ERROR line\nend\n",
    )
    .unwrap();
    run_tzip_create(
        tmp.path(),
        &["create", "aes.zip", "corpus", "-p", "pw", "-q"],
    );

    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["grep", "SECRET", "aes.zip", "-p", "pw"],
        &[],
    );
    assert!(ok, "aes grep failed: {stderr}");
    assert!(stdout.contains("SECRET ERROR line"));
}

// ── HTTP fan-out ───────────────────────────────────────────────────────────

struct ServerGuard {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerGuard {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn serve_files(mapping: Vec<(String, PathBuf)>) -> ServerGuard {
    // Bind the TcpListener first to pin the port, then hand it to
    // tiny_http via from_listener. The old pattern (bind → drop →
    // rebind inside tiny_http) raced with the kernel on macOS and
    // (under CI load) Linux too — between drop and rebind, something
    // else could grab the port and we'd fail with AddrInUse or get a
    // Connection refused on the first HEAD.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

    // Preload all file bytes.
    let files: Vec<(String, Vec<u8>)> = mapping
        .into_iter()
        .map(|(p, path)| (p, fs::read(&path).unwrap()))
        .collect();

    let join = thread::spawn(move || {
        let server = tiny_http::Server::from_listener(listener, None)
            .expect("tiny_http from_listener");
        loop {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let req = match server.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => break,
            };
            let want = req.url().to_string();
            match files.iter().find(|(p, _)| p == &want) {
                Some((_, body)) => serve_body(req, body),
                None => {
                    let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
                }
            }
        }
    });

    ServerGuard {
        addr,
        stop,
        join: Some(join),
    }
}

fn serve_body(req: tiny_http::Request, body: &[u8]) {
    use tiny_http::{Header, Response, StatusCode};
    let is_head = matches!(req.method(), tiny_http::Method::Head);
    let range = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
        .map(|h| h.value.as_str().to_string());

    let header = |n: &str, v: &str| -> Header {
        Header::from_bytes(n.as_bytes(), v.as_bytes()).unwrap()
    };

    if is_head {
        let resp = Response::empty(StatusCode(200))
            .with_header(header("Content-Length", &body.len().to_string()))
            .with_header(header("Accept-Ranges", "bytes"));
        let _ = req.respond(resp);
        return;
    }
    if let Some(r) = range {
        if let Some(rest) = r.strip_prefix("bytes=") {
            if let Some((s, e)) = rest.split_once('-') {
                let start: u64 = s.parse().unwrap_or(0);
                let end: u64 = if e.is_empty() {
                    body.len() as u64 - 1
                } else {
                    e.parse().unwrap_or(0)
                };
                if start < body.len() as u64 && end < body.len() as u64 && start <= end {
                    let slice = &body[start as usize..=end as usize];
                    let resp = Response::from_data(slice.to_vec())
                        .with_status_code(206)
                        .with_header(header("Content-Length", &slice.len().to_string()))
                        .with_header(header(
                            "Content-Range",
                            &format!("bytes {}-{}/{}", start, end, body.len()),
                        ))
                        .with_header(header("Accept-Ranges", "bytes"));
                    let _ = req.respond(resp);
                    return;
                }
            }
        }
    }
    let resp = Response::from_data(body.to_vec())
        .with_status_code(200)
        .with_header(header("Content-Length", &body.len().to_string()))
        .with_header(header("Accept-Ranges", "bytes"));
    let _ = req.respond(resp);
}

#[test]
fn grep_http_parallel_across_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let mut mapping: Vec<(String, PathBuf)> = Vec::new();
    for name in &["a", "b"] {
        let d = tmp.path().join(name);
        fs::create_dir_all(&d).unwrap();
        write_log(
            &d.join("log.txt"),
            &format!("INFO ok {n}\nERROR {n}\n", n = name),
        );
        run_tzip_create(
            tmp.path(),
            &["create", &format!("{}.zip", name), name, "-q"],
        );
        mapping.push((
            format!("/{}.zip", name),
            tmp.path().join(format!("{}.zip", name)),
        ));
    }
    let server = serve_files(mapping);
    let a_url = server.url("/a.zip");
    let b_url = server.url("/b.zip");

    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", &a_url, &b_url, "-P", "2"],
        &[],
    );
    assert!(ok, "http grep failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected output:\n{stdout}");
    assert!(lines.iter().any(|l| l.contains("ERROR a")));
    assert!(lines.iter().any(|l| l.contains("ERROR b")));
}

/// Regression test for issue #54: `tzip grep` on a plain
/// (non-indexed) archive used to hang when the filtered candidate
/// set exceeded the inner fan-out channel's capacity
/// (`inner_jobs * 2` slots, default 16). The producer pre-filled
/// the channel before any worker was spawned, so with ~100+
/// candidates `tx.send` blocked forever.
///
/// Fix: switched the inner fan-out channel from bounded to
/// unbounded. This test exercises the historical hang threshold
/// (200 entries) with a hard timeout — pre-fix tzip stalls past
/// the 30 s cap, post-fix completes in <1 s.
#[test]
fn grep_200_entries_plain_archive_does_not_hang() {
    let tmp = tempfile::tempdir().unwrap();
    let logs = tmp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    for i in 0..200 {
        write_log(
            &logs.join(format!("log_{:06}.json", i)),
            &format!("normal {}\n", i),
        );
    }
    // One entry carries the target token; everything else is
    // noise. Without the index, grep scans every body.
    write_log(&logs.join("log_000042.json"), "OutOfMemoryError in worker\n");
    run_tzip_create(tmp.path(), &["create", "plain.zip", "logs", "-q"]);

    // Spawn tzip with a hard wall-clock deadline. Without the
    // fix this would time out at the shell level; with the fix
    // it finishes in milliseconds.
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["grep", "OutOfMemoryError", "plain.zip"],
        &[],
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "grep took {:?} — likely the pre-#54-fix hang: {}",
        elapsed,
        stderr
    );
    assert!(ok, "grep failed: {stderr}");
    assert!(
        stdout.contains("log_000042.json") && stdout.contains("OutOfMemoryError"),
        "expected one hit, got: {stdout}"
    );
}

/// Regression test for ChatGPT review finding #1: when an archive
/// is built with `--index-terms '*.json'` and the corpus has both
/// `.json` and `.log` entries, `tzip grep` used to silently exclude
/// the `.log` entries from scanning (they're outside the term
/// index's coverage set, so the candidate filter dropped them).
/// Fix: the term index now records which entries it covers; grep
/// scans entries outside the coverage set conservatively.
#[test]
fn grep_scans_entries_outside_term_index_coverage() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    write_log(
        &corpus.join("a.json"),
        r#"{"status":"ok","note":"nothing special"}"#,
    );
    // .log is NOT matched by --index-terms '*.json' → outside the
    // term index's coverage. Must still be scanned.
    write_log(
        &corpus.join("b.log"),
        "operation status: FAILURE in worker-42",
    );
    write_log(&corpus.join("c.log"), "all good");
    run_tzip_create(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "corpus",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "-q",
        ],
    );
    let (ok, stdout, stderr) = run_tzip(tmp.path(), &["grep", "status", "out.zip"], &[]);
    assert!(ok, "grep failed: {stderr}");
    assert!(stdout.contains("a.json"), "a.json match missing: {stdout}");
    assert!(
        stdout.contains("b.log"),
        "b.log should be scanned even though --index-terms excluded it: {stdout}"
    );
    // And we should NOT surface the internal index sentinel.
    assert!(
        !stdout.contains(".tzip/"),
        "`.tzip/*` sentinels must be excluded from user grep: {stdout}"
    );
}

#[test]
fn grep_strict_flag_turns_per_entry_skips_into_nonzero_exit() {
    // Default behaviour (PR #93): per-entry decrypt failures are
    // logged to stderr but the grep exits 0 so partial matches
    // remain useful. `--strict` turns the same skip into a
    // non-zero exit, so pipelines that need an authoritative
    // answer can detect the incompleteness.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    write_log(&corpus.join("a.log"), "ERROR here\n");
    write_log(&corpus.join("b.log"), "ERROR there\n");
    run_tzip_create(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "corpus",
            "-m",
            "deflate",
            "-p",
            "right-password",
            "-q",
        ],
    );

    // Wrong password → every entry fails to decrypt → every
    // entry becomes a per-entry skip. Default mode: exit 0 but
    // the stderr summary line reports the incomplete run.
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["grep", "ERROR", "out.zip", "-p", "wrong-password"],
        &[],
    );
    assert!(ok, "default grep should exit 0 on per-entry skips");
    assert!(
        stderr.contains("incomplete"),
        "expected the 'incomplete' summary line, got stderr: {stderr}"
    );

    // --strict: exit non-zero with the same inputs.
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &[
            "grep",
            "ERROR",
            "out.zip",
            "-p",
            "wrong-password",
            "--strict",
        ],
        &[],
    );
    assert!(!ok, "--strict should exit non-zero when entries skipped");
    assert!(
        stderr.contains("incomplete") || stderr.contains("skipped"),
        "expected skip-summary on stderr under --strict: {stderr}"
    );
}

#[test]
fn grep_json_report_trailer_on_incomplete_run() {
    // --json emits a trailing {"report":{...}} line when the run
    // was incomplete so downstream pipelines can detect missing
    // results without parsing stderr.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    write_log(&corpus.join("a.log"), "ERROR x\n");
    run_tzip_create(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "corpus",
            "-m",
            "deflate",
            "-p",
            "right-password",
            "-q",
        ],
    );

    let (_ok, stdout, _stderr) = run_tzip(
        tmp.path(),
        &[
            "grep",
            "ERROR",
            "out.zip",
            "-p",
            "wrong-password",
            "--json",
        ],
        &[],
    );
    let last = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or("");
    assert!(
        last.starts_with(r#"{"report":"#) && last.contains("\"complete\":false"),
        "expected trailing report line, got: {last}"
    );
}

// Satisfy std::io::Read import.
#[allow(dead_code)]
fn _touch() -> usize {
    std::io::empty().read(&mut [0u8; 1]).unwrap_or(0)
}
