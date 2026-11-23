//! End-to-end `tzip find` tests: body-free metadata queries over
//! one or many archives, exercising the include/exclude glob filters,
//! size filters, `--names-only`, `--json`, and HTTP parallel fan-out.

use std::collections::HashSet;
use std::fs;
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

fn make_corpus(tmp: &Path) {
    fs::create_dir_all(tmp.join("corpus")).unwrap();
    fs::write(tmp.join("corpus/small.log"), b"short log\n").unwrap();
    // 4 KiB of something so --min-size cuts in a stable place.
    let big = "big line\n".repeat(600);
    fs::write(tmp.join("corpus/big.log"), big.as_bytes()).unwrap();
    fs::write(tmp.join("corpus/notes.txt"), b"not a log\n").unwrap();
}

#[test]
fn find_single_archive_tabular() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, stderr) = run_tzip(tmp.path(), &["find", "out.zip"]);
    assert!(ok, "find failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 entries, got:\n{stdout}");
    // Default tabular output: archive \t mtime \t size \t name.
    let names: HashSet<String> = lines
        .iter()
        .map(|l| l.rsplit('\t').next().unwrap().to_string())
        .collect();
    assert!(names.contains("corpus/small.log"));
    assert!(names.contains("corpus/big.log"));
    assert!(names.contains("corpus/notes.txt"));
}

#[test]
fn find_names_only() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(tmp.path(), &["find", "out.zip", "--names-only"]);
    assert!(ok);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert!(l.starts_with("out.zip:"), "not names-only format: {l}");
    }
}

#[test]
fn find_include_glob_filters() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--include", "*.log", "--names-only"],
    );
    assert!(ok);
    assert!(stdout.contains("corpus/small.log"));
    assert!(stdout.contains("corpus/big.log"));
    assert!(!stdout.contains("notes.txt"), "include leaked: {stdout}");
}

#[test]
fn find_exclude_glob_filters() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--exclude", "*.txt", "--names-only"],
    );
    assert!(ok);
    assert!(stdout.contains("corpus/small.log"));
    assert!(stdout.contains("corpus/big.log"));
    assert!(!stdout.contains("notes.txt"));
}

#[test]
fn find_min_size_filter() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--min-size", "1024", "--names-only"],
    );
    assert!(ok);
    // big.log (4800 bytes) stays; small.log and notes.txt drop out.
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected only big.log: {stdout}");
    assert!(lines[0].ends_with("corpus/big.log"));
}

#[test]
fn find_max_size_filter() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--max-size", "100", "--names-only"],
    );
    assert!(ok);
    let names: HashSet<String> = stdout
        .lines()
        .map(|l| l.trim_start_matches("out.zip:").to_string())
        .collect();
    assert!(names.contains("corpus/small.log"));
    assert!(names.contains("corpus/notes.txt"));
    assert!(!names.contains("corpus/big.log"), "big.log leaked: {stdout}");
}

#[test]
fn find_since_until_requires_valid_dates() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // --since well in the past: all entries pass.
    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--since", "1970-01-01", "--names-only"],
    );
    assert!(ok);
    assert_eq!(stdout.lines().count(), 3);

    // --until well in the past: no entry survives.
    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--until", "1980-01-01", "--names-only"],
    );
    assert!(ok);
    assert!(stdout.is_empty(), "expected no entries: {stdout}");
}

#[test]
fn find_json_output_shape() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(tmp.path(), &["find", "out.zip", "--json"]);
    assert!(ok);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert!(v["archive"].is_string());
        assert!(v["entry"].is_string());
        assert!(v["size"].is_u64());
        assert!(v["compressed_size"].is_u64());
        assert!(v["method"].is_u64());
        assert!(v["encrypted"].is_boolean());
        assert!(v["mtime"].is_i64());
        assert!(v["crc32"].is_u64());
    }
}

#[test]
fn find_many_archives_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    for name in &["a", "b", "c"] {
        let d = tmp.path().join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("only.txt"), format!("content {n}\n", n = name)).unwrap();
        run_tzip_create(
            tmp.path(),
            &["create", &format!("{}.zip", name), name, "-q"],
        );
    }
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["find", "a.zip", "b.zip", "c.zip", "-P", "3", "--names-only"],
    );
    assert!(ok, "parallel find failed: {stderr}");
    let archives: HashSet<String> = stdout
        .lines()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    assert_eq!(
        archives,
        HashSet::from(["a.zip".into(), "b.zip".into(), "c.zip".into()])
    );
}

#[test]
fn find_no_matching_entries_is_clean_exit() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["find", "out.zip", "--include", "*.no-such-ext"],
    );
    assert!(ok);
    assert!(stdout.is_empty());
}

#[test]
fn find_alias_f_works() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path());
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let (ok, stdout, _) = run_tzip(tmp.path(), &["f", "out.zip", "--names-only"]);
    assert!(ok);
    assert_eq!(stdout.lines().count(), 3);
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let files: Vec<(String, Vec<u8>)> = mapping
        .into_iter()
        .map(|(p, path)| (p, fs::read(&path).unwrap()))
        .collect();

    let join = thread::spawn(move || {
        let server = tiny_http::Server::http(addr).expect("tiny_http");
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
fn find_http_parallel_across_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let mut mapping: Vec<(String, PathBuf)> = Vec::new();
    for name in &["a", "b"] {
        let d = tmp.path().join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("only.txt"), format!("x {n}\n", n = name)).unwrap();
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
        &["find", &a_url, &b_url, "-P", "2", "--names-only"],
    );
    assert!(ok, "http find failed: {stderr}");
    let archives: HashSet<String> = stdout
        .lines()
        .map(|l| l.rsplit_once(':').unwrap().0.to_string())
        .collect();
    assert_eq!(archives, HashSet::from([a_url.clone(), b_url.clone()]));
}
