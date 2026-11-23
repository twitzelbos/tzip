//! End-to-end `tzip ls / cat / x` over HTTP, served from a local
//! `tiny_http` fixture. Verifies the archive bytes seen through the
//! network match those produced by the local reader for the same
//! archive.
//!
//! Tests are gated on reading Range requests correctly; the fixture
//! implements bytes=start-end and returns 206 + Content-Range on hit,
//! 416 on invalid range, 200 + full body on no Range.

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

fn run_tzip(cwd: &Path, args: &[&str]) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run tzip");
    assert!(
        out.status.success(),
        "tzip failed: args={:?} stderr={}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tzip_run(cwd: &Path, args: &[&str]) -> (bool, Vec<u8>, String) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run tzip");
    (
        out.status.success(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Guard that keeps a tiny_http server running on a dedicated thread until
/// dropped.
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
        // tiny_http recv_timeout (set by the server loop) means the thread
        // will see the stop flag within ~200 ms.
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spin up a tiny_http server that serves `file_path` with full Range
/// support. The server binds to 127.0.0.1 on an ephemeral port.
fn serve_file(file_path: PathBuf, support_ranges: bool) -> ServerGuard {
    // Bind to :0 to pick a free port, then pass that address to tiny_http.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // tiny_http will re-bind on the same port.

    let bytes = fs::read(&file_path).expect("read fixture file");
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

    let join = thread::spawn(move || {
        let server = tiny_http::Server::http(addr).expect("tiny_http server");
        loop {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let req = match server.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => break,
            };
            serve_one(req, &bytes, support_ranges);
        }
    });

    ServerGuard {
        addr,
        stop,
        join: Some(join),
    }
}

fn serve_one(req: tiny_http::Request, body: &[u8], support_ranges: bool) {
    use tiny_http::{Header, Response, StatusCode};

    let is_head = matches!(req.method(), tiny_http::Method::Head);

    // Extract Range header if present.
    let range_hdr = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
        .map(|h| h.value.as_str().to_string());

    if is_head {
        let mut resp = Response::empty(StatusCode(200))
            .with_header(header("Content-Length", &body.len().to_string()));
        if support_ranges {
            resp = resp.with_header(header("Accept-Ranges", "bytes"));
        }
        let _ = req.respond(resp);
        return;
    }

    if !support_ranges {
        // Ignore Range entirely, send the whole body. This exercises the
        // "server doesn't support Range" error path in HttpSource.
        let resp = Response::from_data(body.to_vec())
            .with_status_code(200)
            .with_header(header("Content-Length", &body.len().to_string()));
        let _ = req.respond(resp);
        return;
    }

    if let Some(r) = range_hdr {
        if let Some((start, end)) = parse_bytes_range(&r, body.len() as u64) {
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
        } else {
            let resp = Response::empty(StatusCode(416));
            let _ = req.respond(resp);
            return;
        }
    }

    // No Range header → full body.
    let resp = Response::from_data(body.to_vec())
        .with_status_code(200)
        .with_header(header("Content-Length", &body.len().to_string()))
        .with_header(header("Accept-Ranges", "bytes"));
    let _ = req.respond(resp);
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn parse_bytes_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    // "bytes=start-end", one range only (tzip only ever sends one).
    let rest = raw.strip_prefix("bytes=")?;
    let (s, e) = rest.split_once('-')?;
    let start: u64 = s.parse().ok()?;
    let end: u64 = if e.is_empty() {
        total.saturating_sub(1)
    } else {
        e.parse().ok()?
    };
    if start >= total || end >= total || start > end {
        return None;
    }
    Some((start, end))
}

// ── tests ──────────────────────────────────────────────────────────────────

fn make_corpus(root: &Path) {
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("a.txt"), b"hello a\n").unwrap();
    fs::write(root.join("b.txt"), b"hello b, slightly longer\n").unwrap();
    // Bigger file to exercise multi-chunk Read calls on the HTTP source.
    let big: Vec<u8> = (0..50_000u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect();
    fs::write(root.join("blob.bin"), &big).unwrap();
    fs::write(root.join("nested/c.txt"), b"nested\n").unwrap();
}

#[test]
fn http_ls_matches_local_ls() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let archive = tmp.path().join("out.zip");
    let (_ok, local_stdout, _) = tzip_run(tmp.path(), &["ls", archive.to_str().unwrap()]);
    let server = serve_file(archive.clone(), true);
    let url = server.url("/out.zip");
    let (ok, http_stdout, stderr) = tzip_run(tmp.path(), &["ls", &url]);
    assert!(ok, "http ls failed: {}", stderr);
    assert_eq!(http_stdout, local_stdout, "http ls differs from local ls");
}

#[test]
fn http_cat_deflate_matches_source() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let server = serve_file(tmp.path().join("out.zip"), true);
    let url = server.url("/out.zip");
    let (ok, stdout, stderr) = tzip_run(tmp.path(), &["cat", &url, "corpus/blob.bin"]);
    assert!(ok, "http cat failed: {}", stderr);
    let want = fs::read(src.join("blob.bin")).unwrap();
    assert_eq!(stdout, want, "http cat body differs from source");
}

#[test]
fn http_cat_aes_with_password() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-p", "hunter2", "-q"],
    );

    let server = serve_file(tmp.path().join("out.zip"), true);
    let url = server.url("/out.zip");
    let (ok, stdout, stderr) = tzip_run(
        tmp.path(),
        &["cat", &url, "corpus/a.txt", "-p", "hunter2"],
    );
    assert!(ok, "http cat AES failed: {}", stderr);
    assert_eq!(&stdout, b"hello a\n");
}

#[test]
fn http_extract_whole_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let server = serve_file(tmp.path().join("out.zip"), true);
    let url = server.url("/out.zip");
    let extract_root = tmp.path().join("extracted");
    fs::create_dir(&extract_root).unwrap();

    run_tzip(
        tmp.path(),
        &["x", &url, "--to", extract_root.to_str().unwrap()],
    );

    for name in ["a.txt", "b.txt", "blob.bin", "nested/c.txt"] {
        let got = fs::read(extract_root.join("corpus").join(name))
            .unwrap_or_else(|e| panic!("missing {name} after HTTP extract: {e}"));
        let want = fs::read(src.join(name)).unwrap();
        assert_eq!(got, want, "content mismatch for {name}");
    }
}

#[test]
fn http_rejects_server_without_range_support() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // Server that always returns 200 with the full body (no Range support).
    let server = serve_file(tmp.path().join("out.zip"), false);
    let url = server.url("/out.zip");
    let (ok, _stdout, stderr) = tzip_run(tmp.path(), &["ls", &url]);
    assert!(!ok, "expected failure on Range-less server");
    assert!(
        stderr.to_lowercase().contains("range"),
        "expected a Range-support error, got: {stderr}"
    );
}

#[test]
fn http_rejected_for_create() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    let (ok, _stdout, stderr) = tzip_run(
        tmp.path(),
        &["create", "http://example.invalid/out.zip", "a.txt"],
    );
    assert!(!ok, "create should refuse URL targets");
    assert!(
        stderr.to_lowercase().contains("remote")
            || stderr.to_lowercase().contains("not supported")
            || stderr.to_lowercase().contains("url"),
        "expected a clear remote-write error, got: {stderr}"
    );
}

#[test]
fn http_rejected_for_update() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    let (ok, _stdout, stderr) = tzip_run(
        tmp.path(),
        &["u", "http://example.invalid/out.zip", "a.txt"],
    );
    assert!(!ok, "update should refuse URL targets");
    assert!(
        stderr.to_lowercase().contains("remote")
            || stderr.to_lowercase().contains("not supported")
            || stderr.to_lowercase().contains("url"),
        "expected a clear remote-write error, got: {stderr}"
    );
}

// Keep tiny_http's Read import satisfied without warnings.
#[allow(dead_code)]
fn _touch() -> usize {
    let _: std::io::Empty = std::io::empty();
    let mut buf = [0u8; 1];
    std::io::empty().read(&mut buf).unwrap_or(0)
}
