//! End-to-end tests for `tzip u s3://...` against a mock S3 server
//! (issue #68). The mock implements the subset of the S3 API that
//! the append path exercises:
//!
//! - `GET /<bucket>/<key>` (Range + full), returning ETag
//! - `POST /<bucket>/<key>?uploads` → CreateMultipartUpload
//! - `PUT /<bucket>/<key>?partNumber=N&uploadId=U` → UploadPart
//! - `PUT /<bucket>/<key>?partNumber=N&uploadId=U` with
//!   `x-amz-copy-source` + `x-amz-copy-source-range` + optional
//!   `x-amz-copy-source-if-match` → UploadPartCopy (server-side)
//! - `POST /<bucket>/<key>?uploadId=U` with an XML body + optional
//!   `If-Match` → CompleteMultipartUpload (atomic CAS; 412 on
//!   mismatch)
//! - `DELETE /<bucket>/<key>?uploadId=U` → AbortMultipartUpload
//! - `PUT /<bucket>/<key>` with optional `If-Match` → PutObject
//!   (single-PUT fallback for small archives)
//!
//! State is held in a `Mutex<ServerState>`: the current object
//! body + ETag, and a map of in-progress uploads (each a map of
//! part number → bytes). CompleteMultipartUpload concatenates the
//! parts in order under the If-Match lock, swapping the object
//! atomically from the mock's perspective.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
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

fn run_tzip(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (bool, Vec<u8>, String) {
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
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ── Mock state ─────────────────────────────────────────────────────────────

struct Upload {
    parts: BTreeMap<u32, Vec<u8>>,
}

struct ServerState {
    bucket: String,
    key: String,
    body: Vec<u8>,
    etag: String,
    uploads: BTreeMap<String, Upload>,
    /// Monotonic counter for synthesising ETags — the real S3
    /// hashes the body; we just need a stable, unique opaque
    /// handle.
    etag_counter: u64,
    /// When `true`, `CompleteMultipartUpload` returns 412 and
    /// leaves the object untouched regardless of whether the
    /// client's If-Match actually matches. Lets a test force
    /// the concurrent-writer code path without racing threads.
    force_complete_412: bool,
    /// Per-verb request counts. `get_count` lets the probe-trim
    /// regression test assert that `tzip u` doesn't fan out
    /// into one LFH probe per kept entry.
    get_count: usize,
}

impl ServerState {
    fn fresh_etag(&mut self) -> String {
        self.etag_counter += 1;
        format!("mocketag-{:016x}", self.etag_counter)
    }
}

struct ServerGuard {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<ServerState>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerGuard {
    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }
    fn current_etag(&self) -> String {
        self.state.lock().unwrap().etag.clone()
    }
    fn current_body(&self) -> Vec<u8> {
        self.state.lock().unwrap().body.clone()
    }
    /// Arm the mock to return 412 on the next
    /// `CompleteMultipartUpload`. Lets a test exercise the
    /// atomic-CAS failure path without needing a real concurrent
    /// writer.
    fn force_complete_412(&self) {
        self.state.lock().unwrap().force_complete_412 = true;
    }
    fn upload_count(&self) -> usize {
        self.state.lock().unwrap().uploads.len()
    }
    fn get_count(&self) -> usize {
        self.state.lock().unwrap().get_count
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

/// Spin up the mock on an ephemeral port with `fixture` as the
/// initial object body.
fn serve_mock_s3_mpu(bucket: &str, key: &str, fixture: Vec<u8>) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");

    let state = Arc::new(Mutex::new(ServerState {
        bucket: bucket.to_string(),
        key: key.to_string(),
        body: fixture,
        etag: "mocketag-initial".to_string(),
        uploads: BTreeMap::new(),
        etag_counter: 0,
        force_complete_412: false,
        get_count: 0,
    }));
    let state_srv = Arc::clone(&state);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let upload_id_counter = Arc::new(AtomicU64::new(0));

    let join = thread::spawn(move || {
        let server = tiny_http::Server::from_listener(listener, None)
            .expect("tiny_http from_listener");
        loop {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let req = match server.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => break,
            };
            handle(req, &state_srv, &upload_id_counter);
        }
    });

    ServerGuard {
        addr,
        stop,
        state,
        join: Some(join),
    }
}

fn handle(
    mut req: tiny_http::Request,
    state: &Arc<Mutex<ServerState>>,
    upload_id_counter: &Arc<AtomicU64>,
) {
    // Require an Authorization header on every request — sanity
    // that tzip actually signs things.
    let has_auth = req
        .headers()
        .iter()
        .any(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"));
    if !has_auth {
        let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(403)));
        return;
    }

    let method = req.method().as_str().to_string();
    let url = req.url().to_string();
    let (path, query) = split_path_query(&url);

    // Route by the expected `/<bucket>/<key>` path.
    let want_path = {
        let s = state.lock().unwrap();
        format!("/{}/{}", s.bucket, s.key)
    };
    if path != want_path {
        let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
        return;
    }

    match (method.as_str(), query.as_str()) {
        ("GET", _) => handle_get(req, state),
        ("POST", q) if q == "uploads" || q == "uploads=" => {
            handle_create_mpu(req, state, upload_id_counter)
        }
        ("PUT", q) if q.contains("partNumber=") && q.contains("uploadId=") => {
            let q = q.to_string();
            handle_upload_part(req, state, &q)
        }
        ("POST", q) if q.starts_with("uploadId=") => {
            let q = q.to_string();
            handle_complete_mpu(req, state, &q)
        }
        ("DELETE", q) if q.starts_with("uploadId=") => {
            let q = q.to_string();
            handle_abort_mpu(req, state, &q)
        }
        ("PUT", "") => handle_put_object(req, state),
        _ => {
            let _ = req.respond(
                tiny_http::Response::empty(tiny_http::StatusCode(400))
                    .with_header(header("X-Mock-Why", &format!("unhandled {} ?{}", method, query))),
            );
        }
    }
}

fn handle_get(req: tiny_http::Request, state: &Arc<Mutex<ServerState>>) {
    use tiny_http::{Response, StatusCode};
    let (body, etag) = {
        let mut s = state.lock().unwrap();
        s.get_count += 1;
        (s.body.clone(), s.etag.clone())
    };
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
                .with_header(header("ETag", &format!("\"{}\"", etag)))
                .with_header(header("Accept-Ranges", "bytes"));
            let _ = req.respond(resp);
            return;
        }
        let _ = req.respond(Response::empty(StatusCode(416)));
        return;
    }
    let resp = Response::from_data(body.clone())
        .with_status_code(200)
        .with_header(header("Content-Length", &body.len().to_string()))
        .with_header(header("ETag", &format!("\"{}\"", etag)))
        .with_header(header("Accept-Ranges", "bytes"));
    let _ = req.respond(resp);
}

fn handle_create_mpu(
    req: tiny_http::Request,
    state: &Arc<Mutex<ServerState>>,
    upload_id_counter: &Arc<AtomicU64>,
) {
    let n = upload_id_counter.fetch_add(1, Ordering::SeqCst);
    let upload_id = format!("mockupload-{:08x}", n);
    state
        .lock()
        .unwrap()
        .uploads
        .insert(upload_id.clone(), Upload { parts: BTreeMap::new() });
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult>
<Bucket>mock</Bucket>
<Key>mock</Key>
<UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
        upload_id
    );
    let _ = req.respond(
        tiny_http::Response::from_data(body.into_bytes())
            .with_status_code(200)
            .with_header(header("Content-Type", "application/xml")),
    );
}

fn handle_upload_part(req: tiny_http::Request, state: &Arc<Mutex<ServerState>>, query: &str) {
    let params = query_params(query);
    let part_number: u32 = match params.get("partNumber").and_then(|v| v.parse().ok()) {
        Some(n) => n,
        None => {
            let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(400)));
            return;
        }
    };
    let upload_id = match params.get("uploadId").cloned() {
        Some(u) => u,
        None => {
            let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(400)));
            return;
        }
    };

    // Distinguish UploadPart from UploadPartCopy by presence of
    // `x-amz-copy-source` header.
    let copy_source = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-amz-copy-source"))
        .map(|h| h.value.as_str().to_string());

    if let Some(_src) = copy_source {
        handle_upload_part_copy(req, state, &upload_id, part_number);
    } else {
        handle_upload_part_body(req, state, &upload_id, part_number);
    }
}

fn handle_upload_part_body(
    mut req: tiny_http::Request,
    state: &Arc<Mutex<ServerState>>,
    upload_id: &str,
    part_number: u32,
) {
    let mut body = Vec::new();
    if req.as_reader().read_to_end(&mut body).is_err() {
        let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(400)));
        return;
    }
    let etag = {
        let mut s = state.lock().unwrap();
        let part_etag = s.fresh_etag();
        match s.uploads.get_mut(upload_id) {
            Some(u) => {
                u.parts.insert(part_number, body);
            }
            None => {
                drop(s);
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
                return;
            }
        }
        part_etag
    };
    let _ = req.respond(
        tiny_http::Response::empty(tiny_http::StatusCode(200))
            .with_header(header("ETag", &format!("\"{}\"", etag))),
    );
}

fn handle_upload_part_copy(
    req: tiny_http::Request,
    state: &Arc<Mutex<ServerState>>,
    upload_id: &str,
    part_number: u32,
) {
    // Parse copy-source-range: "bytes=start-end".
    let range_hdr = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-amz-copy-source-range"))
        .map(|h| h.value.as_str().to_string());
    // Optional If-Match on copy source.
    let if_match = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-amz-copy-source-if-match"))
        .map(|h| normalize_etag(h.value.as_str()));

    let (slice, new_part_etag) = {
        let mut s = state.lock().unwrap();
        if let Some(wanted) = if_match {
            if wanted != s.etag {
                drop(s);
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(412)));
                return;
            }
        }
        let body_len = s.body.len() as u64;
        let slice = match range_hdr.as_deref() {
            Some(r) => match parse_bytes_range(r, body_len) {
                Some((start, end)) => s.body[start as usize..=end as usize].to_vec(),
                None => {
                    drop(s);
                    let _ =
                        req.respond(tiny_http::Response::empty(tiny_http::StatusCode(416)));
                    return;
                }
            },
            None => s.body.clone(),
        };
        let part_etag = s.fresh_etag();
        match s.uploads.get_mut(upload_id) {
            Some(u) => {
                u.parts.insert(part_number, slice.clone());
            }
            None => {
                drop(s);
                let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
                return;
            }
        }
        (slice, part_etag)
    };
    let _ = slice; // consumed via u.parts.insert
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CopyPartResult>
<ETag>"{}"</ETag>
</CopyPartResult>"#,
        new_part_etag
    );
    let _ = req.respond(
        tiny_http::Response::from_data(body.into_bytes())
            .with_status_code(200)
            .with_header(header("Content-Type", "application/xml")),
    );
}

fn handle_complete_mpu(
    mut req: tiny_http::Request,
    state: &Arc<Mutex<ServerState>>,
    query: &str,
) {
    let params = query_params(query);
    let upload_id = match params.get("uploadId").cloned() {
        Some(u) => u,
        None => {
            let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(400)));
            return;
        }
    };
    let if_match = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("if-match"))
        .map(|h| normalize_etag(h.value.as_str()));
    let mut _body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut _body);

    let result = {
        let mut s = state.lock().unwrap();
        // Test hook: force a 412 regardless of actual match.
        // Lets us exercise the concurrent-writer path without
        // racing a second client.
        let mismatch = s.force_complete_412
            || if_match.as_deref().is_some_and(|w| w != s.etag);
        if mismatch {
            drop(s);
            let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>PreconditionFailed</Code></Error>"#
                .to_vec();
            let resp = tiny_http::Response::from_data(body)
                .with_status_code(412)
                .with_header(header("Content-Type", "application/xml"));
            let _ = req.respond(resp);
            return;
        }
        let upload = match s.uploads.remove(&upload_id) {
            Some(u) => u,
            None => {
                drop(s);
                let _ =
                    req.respond(tiny_http::Response::empty(tiny_http::StatusCode(404)));
                return;
            }
        };
        let mut composed: Vec<u8> = Vec::new();
        for (_, bytes) in upload.parts {
            composed.extend_from_slice(&bytes);
        }
        let new_etag = s.fresh_etag();
        s.body = composed;
        s.etag = new_etag.clone();
        new_etag
    };

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult>
<Bucket>mock</Bucket>
<Key>mock</Key>
<ETag>"{}"</ETag>
</CompleteMultipartUploadResult>"#,
        result
    );
    let resp = tiny_http::Response::from_data(body.into_bytes())
        .with_status_code(200)
        .with_header(header("Content-Type", "application/xml"))
        .with_header(header("ETag", &format!("\"{}\"", result)));
    let _ = req.respond(resp);
}

fn handle_abort_mpu(req: tiny_http::Request, state: &Arc<Mutex<ServerState>>, query: &str) {
    let params = query_params(query);
    if let Some(upload_id) = params.get("uploadId") {
        state.lock().unwrap().uploads.remove(upload_id);
    }
    let _ = req.respond(tiny_http::Response::empty(tiny_http::StatusCode(204)));
}

fn handle_put_object(mut req: tiny_http::Request, state: &Arc<Mutex<ServerState>>) {
    let if_match = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("if-match"))
        .map(|h| normalize_etag(h.value.as_str()));
    let mut body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body);

    let new_etag = {
        let mut s = state.lock().unwrap();
        if let Some(wanted) = if_match {
            if wanted != s.etag {
                drop(s);
                let _ =
                    req.respond(tiny_http::Response::empty(tiny_http::StatusCode(412)));
                return;
            }
        }
        let new_etag = s.fresh_etag();
        s.body = body;
        s.etag = new_etag.clone();
        new_etag
    };
    let resp = tiny_http::Response::empty(tiny_http::StatusCode(200))
        .with_header(header("ETag", &format!("\"{}\"", new_etag)));
    let _ = req.respond(resp);
}

// Helpers ─────────────────────────────────────────────────────────────────

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

fn split_path_query(url: &str) -> (String, String) {
    match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.to_string(), String::new()),
    }
}

fn query_params(query: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for part in query.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k.to_string(), urldecode(v)),
            None => (part.to_string(), String::new()),
        };
        out.insert(k, v);
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                hex_digit(bytes[i + 1]),
                hex_digit(bytes[i + 2]),
            ) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn normalize_etag(raw: &str) -> String {
    raw.trim_matches('"').to_string()
}

fn env_s3(server: &ServerGuard) -> Vec<(&'static str, String)> {
    vec![
        ("AWS_ENDPOINT_URL", server.endpoint()),
        ("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE".to_string()),
        (
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
        ),
        ("AWS_REGION", "us-east-1".to_string()),
    ]
}

fn env_s3_refs<'a>(v: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    v.iter().map(|(k, val)| (*k, val.as_str())).collect()
}

/// Create a corpus big enough to push the composed `cd_offset +
/// tail_len` past `SINGLE_PUT_MAX_TOTAL` (32 MiB, set in
/// `src/update/s3.rs`), so the multipart (NOT single-PUT and
/// NOT small-prefix multipart) dispatch branch is exercised.
///
/// Before the issue #112 fix a 5-MiB-plus corpus was enough —
/// dispatch keyed only on `cd_offset`. After the fix, dispatch
/// keys on the combined body size, and tests that want the
/// canonical `multipart_append` path (server-side
/// `UploadPartCopy` over a large kept prefix) need > 32 MiB of
/// kept bytes.
fn make_big_corpus(root: &Path) {
    fs::create_dir_all(root).unwrap();
    // 20 × 2 MiB = 40 MiB of raw bodies — comfortably above
    // SINGLE_PUT_MAX_TOTAL so these tests exercise the full
    // `multipart_append` path (UploadPartCopy over the kept
    // prefix).
    for i in 0u32..20 {
        let mut body = vec![0u8; 2 * 1024 * 1024];
        for (j, b) in body.iter_mut().enumerate() {
            *b = (i.wrapping_mul(97).wrapping_add(j as u32)) as u8;
        }
        fs::write(root.join(format!("page_{:03}.bin", i)), &body).unwrap();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn s3_append_mpu_roundtrip() {
    // Build a local archive, serve it at a mock s3:// URL, run
    // `tzip u s3://... new.txt`, then verify:
    //   1. tzip exits 0
    //   2. mock's stored body advanced (new ETag)
    //   3. `tzip ls` sees the new entry alongside the old ones
    //   4. `tzip cat` returns the new entry's content
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    make_big_corpus(&corpus);
    // `-m store`: our fixture bodies are from a cheap PRNG; the
    // default deflate would compress them into < 5 MiB and break
    // the "exercise multipart" invariant. Store keeps the archive
    // roughly the size of the raw corpus.
    run_tzip_create(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    let initial_len = initial_body.len();
    assert!(
        initial_len > 5 * 1024 * 1024,
        "fixture must exceed 5 MiB to exercise multipart; got {}",
        initial_len
    );

    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body.clone());
    let initial_etag = server.current_etag();

    // Write a new file to append.
    fs::write(tmp.path().join("new.txt"), b"hello from s3 append\n").unwrap();

    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(ok, "s3 u failed: {}", stderr);

    // Object body must have changed.
    assert_ne!(
        server.current_etag(),
        initial_etag,
        "server etag didn't advance: {}",
        stderr
    );

    // Entry list must include the new file plus every old one.
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["ls", "s3://mybucket/out.zip"],
        &env_ref,
    );
    assert!(ok, "post-append ls failed: {}", stderr);
    let out = String::from_utf8(stdout).unwrap();
    assert!(
        out.contains("new.txt"),
        "listing missing new.txt:\n{}",
        out
    );
    assert!(
        out.contains("corpus/page_000.bin"),
        "listing dropped old entry:\n{}",
        out
    );
    assert!(
        out.contains("corpus/page_019.bin"),
        "listing dropped last old entry:\n{}",
        out
    );

    // And cat on the new entry returns exactly what we wrote.
    let (ok, stdout, stderr) = run_tzip(
        tmp.path(),
        &["cat", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(ok, "post-append cat failed: {}", stderr);
    assert_eq!(stdout, b"hello from s3 append\n");
}

#[test]
fn s3_append_small_archive_single_put_fallback() {
    // cd_offset below 5 MiB — forces the single-PUT branch. One
    // small entry is enough: the archive will be well under the
    // MIN_PART_SIZE threshold.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    assert!(initial_body.len() < 5 * 1024 * 1024);

    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body);
    let initial_etag = server.current_etag();
    fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "b.txt"],
        &env_ref,
    );
    assert!(ok, "small-archive s3 u failed: {}", stderr);
    assert!(
        stderr.contains("single-PUT") || stderr.contains("5 MiB"),
        "expected stderr to mention the single-PUT fallback: {}",
        stderr
    );
    assert_ne!(server.current_etag(), initial_etag);

    let (ok, stdout, _) = run_tzip(
        tmp.path(),
        &["ls", "s3://mybucket/out.zip"],
        &env_ref,
    );
    assert!(ok);
    let out = String::from_utf8(stdout).unwrap();
    assert!(out.contains("a.txt"));
    assert!(out.contains("b.txt"));
}

#[test]
fn s3_append_small_prefix_large_tail_uses_multipart_not_single_put() {
    // Issue #112 regression. Before the fix, the dispatch used
    // `cd_offset < MIN_PART_SIZE` alone to pick the single-PUT
    // fallback, regardless of how big the new tail was. A tiny
    // seed + huge append body therefore produced a single
    // multi-dozen-MiB PutObject that failed with `broken pipe` in
    // the wild.
    //
    // After the fix, when the combined body exceeds
    // `SINGLE_PUT_MAX_TOTAL` (32 MiB) we take the small-prefix
    // multipart path — part 1 carries the small prefix plus the
    // first 16 MiB of tail, subsequent parts stream the rest from
    // the temp file.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("seed.txt"), b"seed\n").unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "seed.txt", "-q"]);
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    // Sanity: seed well below MIN_PART_SIZE so we exercise the
    // small-prefix branch.
    assert!(
        initial_body.len() < 5 * 1024 * 1024,
        "seed must be < 5 MiB to hit small-prefix branch, got {} bytes",
        initial_body.len()
    );

    // Build ~40 MiB of inputs — comfortably above
    // SINGLE_PUT_MAX_TOTAL (32 MiB).
    let corpus = tmp.path().join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    // 20 × 2 MiB entries = 40 MiB of bodies. Deterministic fill
    // keeps the body incompressible-ish under `-m store`.
    for i in 0u32..20 {
        let mut body = vec![0u8; 2 * 1024 * 1024];
        for (j, b) in body.iter_mut().enumerate() {
            *b = (i.wrapping_mul(37).wrapping_add(j as u32)) as u8;
        }
        fs::write(corpus.join(format!("page_{:03}.bin", i)), &body).unwrap();
    }

    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body);
    let initial_etag = server.current_etag();
    assert_eq!(
        server.upload_count(),
        0,
        "mock should have no in-flight uploads before tzip runs"
    );

    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &[
            "u",
            "s3://mybucket/out.zip",
            "corpus",
            "-m",
            "store",
        ],
        &env_ref,
    );
    assert!(
        ok,
        "small-prefix large-tail s3 u failed: {}",
        stderr
    );
    // Must have taken the issue #112 branch, NOT the single-PUT
    // branch that the old dispatcher would have taken.
    assert!(
        stderr.contains("small-prefix multipart") || stderr.contains("issue #112"),
        "stderr must mention the small-prefix multipart branch: {}",
        stderr
    );
    assert!(
        !stderr.contains("using single-PUT fallback"),
        "must NOT fall back to single-PUT for a > 32 MiB combined body: {}",
        stderr
    );
    // Must have gone through a multipart lifecycle.
    assert!(
        stderr.contains("multipart upload"),
        "stderr must mention multipart upload: {}",
        stderr
    );
    assert_ne!(
        server.current_etag(),
        initial_etag,
        "object ETag should change after successful append"
    );

    // Archive must list ALL entries now — seed + 20 corpus files.
    let (ls_ok, stdout, _) = run_tzip(
        tmp.path(),
        &["ls", "s3://mybucket/out.zip"],
        &env_ref,
    );
    assert!(ls_ok);
    let ls_out = String::from_utf8(stdout).unwrap();
    assert!(ls_out.contains("seed.txt"), "seed missing after append");
    for i in 0u32..20 {
        let name = format!("page_{:03}.bin", i);
        assert!(
            ls_out.contains(&name),
            "{} missing from archive after append",
            name
        );
    }
}

#[test]
fn s3_append_small_prefix_small_tail_still_uses_single_put() {
    // Complementary to the above: ensure we DIDN'T regress the
    // efficient path for everyday small appends. A tiny seed +
    // single-file tail should stay on the single-PUT branch.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    assert!(initial_body.len() < 5 * 1024 * 1024);

    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body);
    fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "b.txt"],
        &env_ref,
    );
    assert!(ok, "small append failed: {}", stderr);
    // Must use single-PUT (not multipart) for the tiny combined body.
    assert!(
        stderr.contains("single-PUT"),
        "small-combined-body append must use single-PUT: {}",
        stderr
    );
    assert!(
        !stderr.contains("multipart upload"),
        "small-combined-body append must NOT escalate to multipart: {}",
        stderr
    );
}

#[test]
fn s3_append_fails_cleanly_on_etag_mismatch_without_mutating_object() {
    // Concurrent-writer scenario: tzip opens the archive and
    // captures ETag=e1, another writer flips the object to e2,
    // then tzip's `CompleteMultipartUpload` arrives carrying
    // If-Match=e1 and the server returns 412.
    //
    // Expected: tzip's `tzip u` exits non-zero and the live
    // object's bytes stay what the second writer left there
    // (NOT tzip's composed new tail). This is issue #68's
    // "ETag mismatch → 412 → original unchanged" case.
    //
    // We can't race two real writers in one subprocess test, so
    // the mock has a `force_complete_412` hook that simulates a
    // stale If-Match regardless of what the client actually
    // sends.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    make_big_corpus(&corpus);
    run_tzip_create(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body.clone());
    let initial_etag = server.current_etag();

    // Arm the mock before tzip runs.
    server.force_complete_412();

    fs::write(tmp.path().join("new.txt"), b"concurrent-writer-scenario\n").unwrap();
    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(!ok, "tzip u must fail when Complete returns 412");
    assert!(
        stderr.contains("CompleteMultipartUpload")
            || stderr.to_lowercase().contains("precondition")
            || stderr.contains("412"),
        "stderr must surface the CAS failure: {}",
        stderr
    );
    // Critical atomicity invariant: the live object is byte-
    // identical to what it was before the failed append.
    assert_eq!(
        server.current_body(),
        initial_body,
        "object body must not change on a 412"
    );
    assert_eq!(
        server.current_etag(),
        initial_etag,
        "object etag must not change on a 412"
    );
}

#[test]
fn s3_append_leaves_no_in_progress_mpu_after_success() {
    // Clean-exit invariant: a successful `tzip u s3://...` must
    // not leave an in-progress multipart upload dangling.
    // CompleteMultipartUpload removes the upload from the
    // server's tracker; Abort does the same on error. Either
    // way, the "in-progress MPUs" count should return to zero.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    make_big_corpus(&corpus);
    run_tzip_create(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body);

    fs::write(tmp.path().join("new.txt"), b"hello\n").unwrap();
    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(ok, "s3 u failed: {}", stderr);
    assert_eq!(
        server.upload_count(),
        0,
        "multipart upload tracker not cleaned up after success"
    );
}

#[test]
fn s3_append_rejects_delete_sort_atomic_and_name_collision() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();

    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body);
    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);

    // --delete: unsupported in v1.
    fs::write(tmp.path().join("new.txt"), b"new\n").unwrap();
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "--delete", "a.txt", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(!ok);
    assert!(
        stderr.contains("--delete") && stderr.contains("not supported"),
        "expected --delete rejection: {}",
        stderr
    );

    // --sort: unsupported.
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "--sort", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(!ok);
    assert!(
        stderr.contains("--sort") && stderr.contains("not supported"),
        "expected --sort rejection: {}",
        stderr
    );

    // Collision with existing entry name.
    fs::write(tmp.path().join("a.txt"), b"replacement\n").unwrap();
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "a.txt"],
        &env_ref,
    );
    assert!(!ok);
    assert!(
        stderr.contains("already exists"),
        "expected collision rejection: {}",
        stderr
    );
}

#[test]
fn s3_append_does_not_probe_lfh_per_kept_entry() {
    // P1 audit: the S3 append path doesn't rewrite any kept
    // entry — it ships `[0, cd_offset)` through a single
    // server-side `UploadPartCopy`. `record_existing` reads
    // only CDR-sourced fields. Before this fix, `tzip u`
    // nonetheless fetched every kept LFH (~30 B each) at
    // Range-GET granularity, so a 1 000-entry archive incurred
    // 1 000+ signed round-trips just to warm up.
    //
    // Assert the count of GETs issued during the append is
    // bounded by a small constant (probe + EOCD-scan +
    // CDR-fetch + a handful of per-CDR-span reads for large
    // CDRs), NOT by the number of kept entries.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    // 400 tiny entries — small enough to run quickly, large
    // enough that a per-entry LFH probe would show up as
    // ≥ 400 GETs.
    for i in 0..400u32 {
        fs::write(corpus.join(format!("e{:04}.txt", i)), b"x").unwrap();
    }
    run_tzip_create(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );
    let initial_body = fs::read(tmp.path().join("out.zip")).unwrap();
    let server = serve_mock_s3_mpu("mybucket", "out.zip", initial_body);

    fs::write(tmp.path().join("new.txt"), b"appended\n").unwrap();
    let env = env_s3(&server);
    let env_ref = env_s3_refs(&env);
    let (ok, _stdout, stderr) = run_tzip(
        tmp.path(),
        &["u", "s3://mybucket/out.zip", "new.txt"],
        &env_ref,
    );
    assert!(ok, "s3 u failed: {stderr}");

    // Every GET here is one signed Range round-trip. For 400
    // kept entries the pre-fix code path produced ~400+; the
    // post-fix shape is probe + CDR-fetch(es), i.e. a small
    // handful. Give generous slack so a future prefetch tweak
    // or an extra EOCD64-locator read doesn't false-fail; the
    // invariant that matters is "sub-linear in entry count."
    let gets = server.get_count();
    assert!(
        gets < 50,
        "tzip u issued {gets} Range GETs against a 400-entry \
         archive; append path must be O(1), not O(entries)",
    );
}

#[allow(dead_code)]
fn _touch() -> usize {
    std::io::empty().read(&mut [0u8; 1]).unwrap_or(0)
}
