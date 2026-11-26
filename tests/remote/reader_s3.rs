//! End-to-end `tzip ls / cat / x` against a mock S3 server. The server is
//! a `tiny_http` fixture that answers path-style GETs (`/<bucket>/<key>`)
//! with Range support; signature validation is covered separately by the
//! SigV4 unit tests in `src/sources/sigv4.rs`.
//!
//! These tests set `AWS_ENDPOINT_URL=http://127.0.0.1:PORT` to redirect the
//! S3 client at the fixture. They also set fake credentials so the client's
//! credential chain succeeds.

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

fn run_tzip(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (bool, Vec<u8>, String) {
    let mut cmd = Command::new(tzip_bin());
    cmd.current_dir(cwd).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run tzip");
    (
        out.status.success(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
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

struct ServerGuard {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerGuard {
    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
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

/// Mock S3 server: serves any GET under `/<bucket>/...` with the archive
/// bytes, respecting Range. Accepts the Authorization header verbatim —
/// signature validation is the SigV4 unit tests' job.
fn serve_mock_s3(bucket: &str, key: &str, file_path: PathBuf) -> ServerGuard {
    // Hand the already-bound listener straight to tiny_http so the
    // port never gets released between bind and rebind — the old
    // drop-and-rebind dance was a TOCTOU race that occasionally lost
    // the port to a concurrent test on Linux CI (EADDRINUSE).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");

    let want_path = format!("/{}/{}", bucket, key);
    let body = fs::read(&file_path).expect("read fixture");
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

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
            // Require the Authorization header exists — basic sanity that
            // tzip is actually signing requests.
            let has_auth = req
                .headers()
                .iter()
                .any(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"));
            if !has_auth {
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(403)));
                continue;
            }
            // Only serve the expected object; anything else is 404.
            if req.url() != want_path {
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
                continue;
            }
            serve_bytes(req, &body);
        }
    });

    ServerGuard {
        addr,
        stop,
        join: Some(join),
    }
}

fn serve_bytes(req: tiny_http::Request, body: &[u8]) {
    serve_bytes_with_etag(req, body, "\"etag-v1\"", None)
}

/// Serve `body` with a response ETag of `etag`. If
/// `reject_if_match` is `Some(old_etag)` and the request carries
/// `If-Match: <old_etag>`, respond 412 — the semantic of "the
/// object you opened has been replaced; your precondition no
/// longer matches." Used by the snapshot-swap test.
fn serve_bytes_with_etag(
    req: tiny_http::Request,
    body: &[u8],
    etag: &str,
    reject_if_match: Option<&str>,
) {
    use tiny_http::{Response, StatusCode};

    if let Some(old_etag) = reject_if_match {
        let im = req
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("if-match"))
            .map(|h| h.value.as_str().to_string());
        if let Some(sent) = im {
            if sent == old_etag {
                let _ = req.respond(Response::empty(StatusCode(412)));
                return;
            }
        }
    }

    let range_hdr = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
        .map(|h| h.value.as_str().to_string());

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
                .with_header(header("Accept-Ranges", "bytes"))
                .with_header(header("ETag", etag));
            let _ = req.respond(resp);
            return;
        }
        let _ = req.respond(Response::empty(StatusCode(416)));
        return;
    }

    let resp = Response::from_data(body.to_vec())
        .with_status_code(200)
        .with_header(header("Content-Length", &body.len().to_string()))
        .with_header(header("Accept-Ranges", "bytes"))
        .with_header(header("ETag", etag));
    let _ = req.respond(resp);
}

/// Mock S3 that starts returning HTTP 412 on any GET carrying an
/// `If-Match: "etag-v1"` header after the first N requests. Lets
/// tests exercise "the object was replaced mid-run" without racing
/// a real swap.
fn serve_mock_s3_with_etag_swap(
    bucket: &str,
    key: &str,
    file_path: PathBuf,
    requests_before_swap: usize,
) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let want_path = format!("/{}/{}", bucket, key);
    let body = fs::read(&file_path).expect("read fixture");
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

    let join = thread::spawn(move || {
        let server = tiny_http::Server::from_listener(listener, None)
            .expect("tiny_http from_listener");
        let mut served = 0usize;
        loop {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let req = match server.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => break,
            };
            let has_auth = req
                .headers()
                .iter()
                .any(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"));
            if !has_auth {
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(403)));
                continue;
            }
            if req.url() != want_path {
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
                continue;
            }
            served += 1;
            if served > requests_before_swap {
                // After the configured threshold, pretend the
                // object has been replaced: refuse any request
                // whose `If-Match` is the v1 ETag.
                serve_bytes_with_etag(req, &body, "\"etag-v2\"", Some("\"etag-v1\""));
            } else {
                serve_bytes(req, &body);
            }
        }
    });

    ServerGuard {
        addr,
        stop,
        join: Some(join),
    }
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn parse_bytes_range(raw: &str, total: u64) -> Option<(u64, u64)> {
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
    fs::write(root.join("b.txt"), b"content for b\n").unwrap();
    let big: Vec<u8> = (0..20_000u32)
        .map(|i| (i.wrapping_mul(17).wrapping_add(3)) as u8)
        .collect();
    fs::write(root.join("blob.bin"), &big).unwrap();
}

#[test]
fn s3_ls_against_mock_server() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let server = serve_mock_s3("mybucket", "out.zip", tmp.path().join("out.zip"));
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["ls", "s3://mybucket/out.zip"],
        &[
            ("AWS_ENDPOINT_URL", &server.endpoint()),
            ("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            ("AWS_REGION", "us-east-1"),
        ],
    );
    assert!(ok, "s3 ls failed: {}", stderr);
    let out = String::from_utf8(stdout).unwrap();
    assert!(out.contains("corpus/a.txt"));
    assert!(out.contains("corpus/b.txt"));
    assert!(out.contains("corpus/blob.bin"));
}

#[test]
fn s3_cat_against_mock_server() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let server = serve_mock_s3("mybucket", "out.zip", tmp.path().join("out.zip"));
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["cat", "s3://mybucket/out.zip", "corpus/blob.bin"],
        &[
            ("AWS_ENDPOINT_URL", &server.endpoint()),
            ("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            ("AWS_REGION", "us-east-1"),
        ],
    );
    assert!(ok, "s3 cat failed: {}", stderr);
    let want = fs::read(src.join("blob.bin")).unwrap();
    assert_eq!(stdout, want);
}

#[test]
fn s3_credentials_error_when_env_unset() {
    let tmp = tempfile::tempdir().unwrap();
    // Point at a non-existent HOME so ~/.aws/credentials isn't consulted.
    let fake_home = tmp.path().join("no-such-home");
    fs::create_dir_all(&fake_home).unwrap();

    let (ok, _stdout, stderr) = {
        // Explicitly unset the AWS creds env vars for this subprocess by not
        // forwarding them. Command::env_clear() would kill PATH etc; instead
        // we set the var names to empty and let the code's `env::var` fail
        // (empty string still counts as set — so use `env_remove`).
        let mut cmd = Command::new(tzip_bin());
        cmd.current_dir(tmp.path())
            .args(["ls", "s3://any-bucket/any-key"])
            .env("HOME", &fake_home)
            .env("AWS_ENDPOINT_URL", "http://127.0.0.1:1")
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("AWS_SESSION_TOKEN")
            .env_remove("AWS_PROFILE")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = cmd.output().expect("run tzip");
        (
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    assert!(!ok, "expected failure when no credentials are available");
    assert!(
        stderr.to_lowercase().contains("credentials")
            || stderr.to_lowercase().contains("access_key"),
        "expected a credential error, got: {stderr}"
    );
}

#[test]
fn s3_rejected_for_create() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["create", "s3://bucket/out.zip", "a.txt"],
        &[],
    );
    assert!(!ok);
    assert!(
        stderr.to_lowercase().contains("remote") || stderr.to_lowercase().contains("url"),
        "stderr: {stderr}"
    );
}

#[test]
fn s3_snapshot_change_mid_run_is_detected() {
    // P0 #3: once tzip has captured the object's ETag at open
    // time, every subsequent Range GET sends `If-Match` so a
    // mid-run replacement surfaces as HTTP 412 and bails loudly
    // instead of silently stitching bytes from two versions.
    //
    // The mock: the probe request succeeds and returns ETag
    // "etag-v1" (which the client stores as its If-Match
    // precondition). Every follow-up GET that arrives with
    // `If-Match: "etag-v1"` gets a 412 back — simulating "the
    // object has been replaced since you opened it."
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    // 1 request before the swap covers only the open probe; the
    // CDR fetch that follows will be the 412 we're looking for.
    let server = serve_mock_s3_with_etag_swap(
        "mybucket",
        "out.zip",
        tmp.path().join("out.zip"),
        1,
    );

    // tzip ls does probe → EOCD-tail GET → CDR GET; at least one
    // of the tail reads will land past the swap threshold.
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["ls", "s3://mybucket/out.zip"],
        &[
            ("AWS_ENDPOINT_URL", &server.endpoint()),
            ("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            ("AWS_REGION", "us-east-1"),
        ],
    );
    assert!(!ok, "expected ls to fail after snapshot swap");
    assert!(
        stderr.contains("remote snapshot changed mid-run"),
        "expected snapshot-changed error, got: {stderr}"
    );
}

#[allow(dead_code)]
fn _touch() -> usize {
    std::io::empty().read(&mut [0u8; 1]).unwrap_or(0)
}
