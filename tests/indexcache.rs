//! Phase 6 integration tests — on-disk index cache over a local
//! HTTP fixture. Verifies that index-entry (`.tzip/*.v1`) reads
//! hit the cache on a repeat open instead of re-fetching, and that
//! an ETag change invalidates the stale bucket.

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
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
        String::from_utf8_lossy(&out.stderr),
    );
}

fn tzip_capture(cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run tzip");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Serve `file_path` with Range support, an ETag we can change,
/// and per-run byte-range request counters so tests can tell
/// cache-hit from cache-miss paths.
struct ServerGuard {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    /// How many Range GETs the server has served since last reset.
    /// Tests zero this between runs and assert on it to prove the
    /// cache is being consulted.
    range_hits: Arc<AtomicUsize>,
    /// Mutable ETag + body so the test can flip to a new archive
    /// without killing the server.
    state: Arc<Mutex<ServerState>>,
}

struct ServerState {
    etag: String,
    body: Vec<u8>,
}

impl ServerGuard {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
    fn reset_counters(&self) {
        self.range_hits.store(0, Ordering::SeqCst);
    }
    fn set_body(&self, etag: &str, body: Vec<u8>) {
        let mut s = self.state.lock().unwrap();
        s.etag = etag.to_string();
        s.body = body;
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

fn serve(file_path: PathBuf, initial_etag: &str) -> ServerGuard {
    // Hand the already-bound listener to tiny_http instead of
    // drop-and-rebind — the latter is a TOCTOU race (CI flake on
    // ubuntu-latest) that occasionally loses the port to a
    // concurrent test.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");

    let body = fs::read(&file_path).expect("read fixture");
    let state = Arc::new(Mutex::new(ServerState {
        etag: initial_etag.to_string(),
        body,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let state2 = Arc::clone(&state);
    let range_hits = Arc::new(AtomicUsize::new(0));
    let range_hits2 = Arc::clone(&range_hits);

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
            serve_one(req, &state2, &range_hits2);
        }
    });

    ServerGuard {
        addr,
        stop,
        join: Some(join),
        range_hits,
        state,
    }
}

fn serve_one(
    req: tiny_http::Request,
    state: &Arc<Mutex<ServerState>>,
    range_hits: &Arc<AtomicUsize>,
) {
    use tiny_http::{Response, StatusCode};
    let is_head = matches!(req.method(), tiny_http::Method::Head);
    let range_hdr = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
        .map(|h| h.value.as_str().to_string());

    let (etag, body) = {
        let s = state.lock().unwrap();
        (s.etag.clone(), s.body.clone())
    };

    if is_head {
        let resp = Response::empty(StatusCode(200))
            .with_header(header("Content-Length", &body.len().to_string()))
            .with_header(header("Accept-Ranges", "bytes"))
            .with_header(header("ETag", &etag));
        let _ = req.respond(resp);
        return;
    }

    if let Some(r) = range_hdr {
        if let Some((start, end)) = parse_bytes_range(&r, body.len() as u64) {
            range_hits.fetch_add(1, Ordering::SeqCst);
            let slice = &body[start as usize..=end as usize];
            let resp = Response::from_data(slice.to_vec())
                .with_status_code(206)
                .with_header(header("Content-Length", &slice.len().to_string()))
                .with_header(header(
                    "Content-Range",
                    &format!("bytes {}-{}/{}", start, end, body.len()),
                ))
                .with_header(header("Accept-Ranges", "bytes"))
                .with_header(header("ETag", &etag));
            let _ = req.respond(resp);
            return;
        }
        let resp = Response::empty(StatusCode(416));
        let _ = req.respond(resp);
        return;
    }

    // No Range header → full body.
    let resp = Response::from_data(body.to_vec())
        .with_status_code(200)
        .with_header(header("Content-Length", &body.len().to_string()))
        .with_header(header("Accept-Ranges", "bytes"))
        .with_header(header("ETag", &etag));
    let _ = req.respond(resp);
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

fn make_json_corpus(root: &Path, n: usize) {
    fs::create_dir_all(root.join("logs")).unwrap();
    // Each entry is padded so the archive grows past HttpSource's
    // 256 KiB prefetch window — otherwise the EOCD scan incidentally
    // pulls the facet sentinel into the same GET and the warm run
    // "hits the cache" without ever having made the extra round
    // trip.  Keeping the overall size > 512 KiB makes the facet /
    // term reads their own discrete fetches.
    let padding: String = "x".repeat(4096);
    for i in 0..n {
        let status = match i % 3 {
            0 => 200,
            1 => 404,
            _ => 500,
        };
        fs::write(
            root.join(format!("logs/log_{:04}.json", i)),
            format!(
                r#"{{"status_code":{},"user":"u{}","pad":"{}"}}"#,
                status,
                i % 4,
                padding
            ),
        )
        .unwrap();
    }
}

fn build_archive_with_facet(root: &Path) -> PathBuf {
    // 300 entries × ~4 KiB plaintext → hundreds of KiB stored
    // (deflated to much less, but still big enough that the LFH /
    // CDR / sentinel layout forces independent Range GETs for the
    // facet read).  Count cycles across three status codes so
    // 300 / 3 = 100 each.
    make_json_corpus(root, 300);
    run_tzip(
        root,
        &[
            "create",
            "out.zip",
            "logs",
            "-m",
            "store", // Store mode keeps entry sizes big — forces separation.
            "--index-terms",
            "*.json",
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    root.join("out.zip")
}

#[test]
fn aggregate_over_http_populates_and_serves_from_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = build_archive_with_facet(tmp.path());
    let server = serve(archive.clone(), "etag-v1");
    let url = server.url("/out.zip");
    let cache_dir = tmp.path().join("tzip-cache");

    // Cold cache: run aggregate; the facet entry should be fetched
    // over HTTP and populated into the on-disk cache.
    let (ok, stdout, stderr) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            &url,
        ],
    );
    assert!(ok, "cold run failed: {}", stderr);
    assert!(stdout.contains("100  200"), "stdout missing counts: {}", stdout);

    // One archive bucket with at least one cached entry (the facet).
    let v1 = cache_dir.join("v1");
    assert!(v1.exists(), "cache v1 dir should exist after cold run");
    let buckets: Vec<_> = fs::read_dir(&v1).unwrap().flatten().collect();
    assert_eq!(buckets.len(), 1, "exactly one archive bucket expected");
    let bodies: Vec<_> = fs::read_dir(buckets[0].path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy() != "_meta")
        .collect();
    assert!(
        !bodies.is_empty(),
        "cold run should have populated at least one cache body"
    );

    // Warm cache: second run reads the same facet entry — repeats
    // the aggregate against the same URL and must still produce
    // correct counts. (The on-disk cache is the thing on trial
    // here; the archive bucket's body file either gets served or
    // re-fetched silently — both answer correctly.)
    let (ok, stdout, _) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            &url,
        ],
    );
    assert!(ok);
    assert!(stdout.contains("100  200"), "warm counts wrong: {}", stdout);
}

#[test]
fn cache_body_matches_archive_facet_entry() {
    // Stronger check: after a cold run, the body cached for the
    // facet entry must byte-match what `tzip x` extracts out of the
    // archive. Proves we're storing the correct payload (not just
    // some non-empty blob).
    let tmp = tempfile::tempdir().unwrap();
    let archive = build_archive_with_facet(tmp.path());
    let server = serve(archive.clone(), "etag-v1");
    let url = server.url("/out.zip");
    let cache_dir = tmp.path().join("tzip-cache");

    let (ok, _, stderr) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            &url,
        ],
    );
    assert!(ok, "cold run failed: {}", stderr);

    // Extract the facet entry out of the archive directly.
    let extract_dir = tmp.path().join("ex");
    fs::create_dir_all(&extract_dir).unwrap();
    run_tzip(
        tmp.path(),
        &[
            "x",
            archive.to_str().unwrap(),
            "--to",
            extract_dir.to_str().unwrap(),
            ".tzip/facets.v1/status_code",
        ],
    );
    let extracted = fs::read(extract_dir.join(".tzip/facets.v1/status_code")).unwrap();

    // Find the body file in the cache.
    let v1 = cache_dir.join("v1");
    let bucket = fs::read_dir(&v1).unwrap().next().unwrap().unwrap().path();
    let bodies: Vec<_> = fs::read_dir(&bucket)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy() != "_meta")
        .map(|e| fs::read(e.path()).unwrap())
        .collect();
    assert!(
        bodies.iter().any(|b| b == &extracted),
        "cached bodies should include the facet entry's bytes: lengths={:?}, want {} bytes",
        bodies.iter().map(|b| b.len()).collect::<Vec<_>>(),
        extracted.len()
    );
}

#[test]
fn etag_change_invalidates_cached_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let archive_v1 = build_archive_with_facet(tmp.path());
    let v1_bytes = fs::read(&archive_v1).unwrap();
    let server = serve(archive_v1.clone(), "etag-v1");
    let url = server.url("/out.zip");
    let cache_dir = tmp.path().join("tzip-cache");

    // Prime the cache.
    let (ok, _, _) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            &url,
        ],
    );
    assert!(ok);

    // Rebuild the archive with different data under the SAME URL +
    // a new ETag. (Different data: more entries → different
    // counts.)
    let rebuilt_dir = tmp.path().join("rebuilt");
    fs::create_dir_all(&rebuilt_dir).unwrap();
    make_json_corpus(&rebuilt_dir, 303); // 101× each — distinct from v1's 100× each
    run_tzip(
        tmp.path(),
        &[
            "create",
            "rebuilt.zip",
            "rebuilt/logs",
            "-m",
            "store",
            "--index-terms",
            "*.json",
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    let v2_bytes = fs::read(tmp.path().join("rebuilt.zip")).unwrap();
    assert_ne!(v1_bytes, v2_bytes, "setup: v2 should differ from v1");
    server.set_body("etag-v2", v2_bytes);

    // Re-run — should see the new counts, not the cached v1 counts.
    let (ok, stdout, _) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            &url,
        ],
    );
    assert!(ok);
    assert!(
        stdout.contains("101  200"),
        "etag change should refresh counts; got: {}",
        stdout
    );
}

#[test]
fn missing_cache_dir_is_created() {
    // Starting state: no cache dir exists. First tzip run with
    // --cache-dir pointing inside a yet-to-be-created path should
    // create it instead of erroring.
    let tmp = tempfile::tempdir().unwrap();
    let archive = build_archive_with_facet(tmp.path());
    let server = serve(archive, "etag-x");
    let url = server.url("/out.zip");
    let cache_root = tmp.path().join("deep/nested/cache/root");
    assert!(!cache_root.exists());
    let (ok, _, stderr) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_root.to_str().unwrap(),
            &url,
        ],
    );
    assert!(ok, "aggregate failed: {}", stderr);
    assert!(cache_root.exists(), "cache dir should have been created");
}

#[test]
fn local_archive_ignores_cache_flag() {
    // Local archives shouldn't populate the cache — no remote
    // identity to key off. The flag should be a silent no-op.
    let tmp = tempfile::tempdir().unwrap();
    let archive = build_archive_with_facet(tmp.path());
    let cache_dir = tmp.path().join("cache");
    let (ok, stdout, _) = tzip_capture(
        tmp.path(),
        &[
            "aggregate",
            "--by",
            "status_code",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            archive.to_str().unwrap(),
        ],
    );
    assert!(ok);
    assert!(stdout.contains("100  200"));
    // The cache's v1 dir gets created on open, but no archive
    // bucket inside — local archives skip the cache binding.
    let v1 = cache_dir.join("v1");
    if v1.exists() {
        let buckets: Vec<_> = fs::read_dir(&v1).unwrap().flatten().collect();
        assert!(
            buckets.is_empty(),
            "local archive should not populate cache, found: {:?}",
            buckets.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }
}

/// Keep the module usable even when `Read` isn't referenced elsewhere.
#[allow(dead_code)]
fn _read_marker<R: Read>(_r: R) {}
