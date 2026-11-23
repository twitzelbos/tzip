//! Parallel `tzip x`: many-entry fan-out on local and HTTP archives,
//! AES-under-parallel, `-P 1` sanity, `--connections` cap, and the
//! fail-fast-on-typo'd-names behavior. Mirrors the tiny_http fixture
//! pattern from `tests/grep.rs`/`tests/find.rs`.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread;

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

fn run_tzip_create(cwd: &Path, args: &[&str]) {
    let (ok, _, stderr) = run_tzip(cwd, args);
    assert!(ok, "create failed: {stderr}");
}

fn make_many_entry_corpus(root: &Path, n: usize) -> Vec<(String, Vec<u8>)> {
    fs::create_dir_all(root.join("corpus")).unwrap();
    let mut spec: Vec<(String, Vec<u8>)> = Vec::with_capacity(n);
    for i in 0..n {
        // Size varies so no two entries decompress identically.
        let body = format!("entry {i} payload\n").repeat(1 + (i % 20));
        let name = format!("corpus/file_{:03}.txt", i);
        fs::write(root.join(&name), body.as_bytes()).unwrap();
        spec.push((name, body.into_bytes()));
    }
    spec
}

#[test]
fn extract_parallel_many_entries_local() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = make_many_entry_corpus(tmp.path(), 40);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["x", "out.zip", "--to", out_arg, "-P", "8"],
    );
    assert!(ok, "parallel extract failed: {stderr}");

    for (name, body) in &expected {
        let p = out_dir.join(name);
        let got = fs::read(&p).unwrap_or_else(|_| panic!("missing {}", p.display()));
        assert_eq!(&got, body, "body mismatch on {name}");
    }
}

#[test]
fn extract_parallel_selected_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = make_many_entry_corpus(tmp.path(), 20);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    // Extract only three specific entries at -P 4.
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &[
            "x",
            "out.zip",
            "corpus/file_003.txt",
            "corpus/file_010.txt",
            "corpus/file_019.txt",
            "--to",
            out_arg,
            "-P",
            "4",
        ],
    );
    assert!(ok, "selected-extract failed: {stderr}");

    let wanted: HashSet<&str> = ["corpus/file_003.txt", "corpus/file_010.txt", "corpus/file_019.txt"]
        .into_iter()
        .collect();
    for (name, body) in &expected {
        let p = out_dir.join(name);
        if wanted.contains(name.as_str()) {
            let got = fs::read(&p).unwrap_or_else(|_| panic!("missing {}", p.display()));
            assert_eq!(&got, body);
        } else {
            assert!(!p.exists(), "unwanted entry {} was extracted", name);
        }
    }
}

#[test]
fn extract_fails_fast_on_missing_name() {
    let tmp = tempfile::tempdir().unwrap();
    make_many_entry_corpus(tmp.path(), 3);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["x", "out.zip", "corpus/does_not_exist.txt", "--to", out_arg],
    );
    assert!(!ok, "expected failure on missing name");
    assert!(
        stderr.contains("not in archive"),
        "unexpected stderr: {stderr}"
    );
    // Nothing partial should have been written.
    assert_eq!(
        fs::read_dir(&out_dir).unwrap().count(),
        0,
        "partial files leaked on failed extract"
    );
}

#[test]
fn extract_single_worker_matches_default() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = make_many_entry_corpus(tmp.path(), 10);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["x", "out.zip", "--to", out_arg, "-P", "1"],
    );
    assert!(ok, "-P 1 extract failed: {stderr}");
    for (name, body) in &expected {
        let got = fs::read(out_dir.join(name)).unwrap();
        assert_eq!(&got, body);
    }
}

#[test]
fn extract_parallel_aes() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = make_many_entry_corpus(tmp.path(), 15);
    run_tzip_create(
        tmp.path(),
        &["create", "aes.zip", "corpus", "-p", "pw", "-q"],
    );

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["x", "aes.zip", "--to", out_arg, "-p", "pw", "-P", "6"],
    );
    assert!(ok, "AES parallel extract failed: {stderr}");
    for (name, body) in &expected {
        let got = fs::read(out_dir.join(name)).unwrap();
        assert_eq!(&got, body);
    }
}

#[test]
fn extract_connections_caps_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = make_many_entry_corpus(tmp.path(), 8);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    // --connections 1 should still succeed, just with a single worker.
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &[
            "x",
            "out.zip",
            "--to",
            out_arg,
            "-P",
            "16",
            "--connections",
            "1",
        ],
    );
    assert!(ok, "connections-capped extract failed: {stderr}");
    for (name, body) in &expected {
        assert_eq!(&fs::read(out_dir.join(name)).unwrap(), body);
    }
}

// ── HTTP fan-out ───────────────────────────────────────────────────────────

struct ServerGuard {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
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
    let hits = Arc::new(AtomicUsize::new(0));
    let stop2 = Arc::clone(&stop);
    let hits2 = Arc::clone(&hits);

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
            hits2.fetch_add(1, Ordering::Relaxed);
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
        hits,
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
fn extract_parallel_http_fan_out() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = make_many_entry_corpus(tmp.path(), 25);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let server = serve_files(vec![("/out.zip".into(), tmp.path().join("out.zip"))]);
    let url = server.url("/out.zip");
    let baseline_hits = server.hits.load(Ordering::Relaxed);

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["x", &url, "--to", out_arg, "-P", "6"],
    );
    assert!(ok, "http parallel extract failed: {stderr}");

    for (name, body) in &expected {
        let got = fs::read(out_dir.join(name)).unwrap();
        assert_eq!(&got, body, "mismatch on {name}");
    }

    // Each of 6 workers opens its own HttpSource (HEAD + Range probe
    // + body GETs). HttpSource has a read-through cache that groups
    // adjacent reads, so small entries can share a GET — the exact
    // count is implementation-defined. The real correctness proof is
    // that all 25 entries extracted with matching bytes (above). This
    // weaker check just confirms we actually did per-worker HTTP work
    // rather than, say, everything from one shared socket.
    let hits = server.hits.load(Ordering::Relaxed) - baseline_hits;
    assert!(hits >= 6, "expected at least 6 HTTP hits (one per worker open), got {hits}");
}

// Satisfy an otherwise-unused Read import on some toolchains.
#[allow(dead_code)]
fn _touch() -> usize {
    std::io::empty().read(&mut [0u8; 1]).unwrap_or(0)
}
