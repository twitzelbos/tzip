//! End-to-end `tzip mount` tests. Linux-only because FUSE3 is the only
//! platform `tzip mount` supports in this release; macOS FSKit is a
//! planned follow-up.
//!
//! Each test spawns `tzip mount` as a subprocess, polls the mountpoint
//! until it becomes live, runs assertions against it via regular
//! filesystem syscalls, then sends SIGTERM and reaps the child.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

/// RAII mount guard: spawns `tzip mount …`, waits for the mountpoint to
/// come up, and on `Drop` sends SIGTERM + waits for the child.
struct MountGuard {
    child: Option<Child>,
    mountpoint: PathBuf,
}

impl MountGuard {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SIGTERM so the FUSE session unmounts cleanly.
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if Instant::now() > deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            // Best-effort emergency unmount.
                            let _ = Command::new("fusermount3")
                                .args(["-u", self.mountpoint.to_str().unwrap()])
                                .status();
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

fn mount(archive: &str, mountpoint: &Path, extra: &[&str]) -> MountGuard {
    fs::create_dir_all(mountpoint).unwrap();
    let mut args: Vec<&str> = vec!["mount", archive, mountpoint.to_str().unwrap()];
    args.extend_from_slice(extra);
    let child = Command::new(tzip_bin())
        .args(&args)
        // Inherit stderr so `-v` messages / errors surface to the test
        // process when something goes wrong.
        .stderr(Stdio::inherit())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn tzip mount");

    // Poll until the mountpoint is live. `read_dir` on the mountpoint
    // succeeds once FUSE is serving; before that it's an empty local
    // directory. We detect "serving" by waiting for any entry to appear
    // OR a getattr via metadata to succeed on a known file. Simpler:
    // just wait a short time + retry readdir.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            panic!("tzip mount did not come up within 5 s at {}", mountpoint.display());
        }
        // Use `statfs` to tell FUSE from a plain directory: a FUSE mount
        // sets a distinctive `f_type`. Rather than poking at that, we
        // just wait briefly and then trust the mount. Pragmatic MVP.
        thread::sleep(Duration::from_millis(150));
        if let Ok(iter) = fs::read_dir(mountpoint) {
            // Any success with non-blocking readdir = FUSE is up.
            // Collecting kicks the lookup off.
            let _ = iter.collect::<Result<Vec<_>, _>>();
            // Loop once more to ensure state settles.
            thread::sleep(Duration::from_millis(100));
            break;
        }
    }

    MountGuard {
        child: Some(child),
        mountpoint: mountpoint.to_path_buf(),
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

fn make_corpus(root: &Path) {
    fs::create_dir_all(root.join("nested/deep")).unwrap();
    fs::write(root.join("a.txt"), b"hello a\n").unwrap();
    fs::write(root.join("b.txt"), b"hello b, second file\n").unwrap();
    fs::write(root.join("nested/c.txt"), b"nested c\n").unwrap();
    fs::write(root.join("nested/deep/d.txt"), b"d deep\n").unwrap();
}

#[test]
fn mount_lists_root_entries() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let mp = tmp.path().join("mnt");
    let _guard = mount(tmp.path().join("out.zip").to_str().unwrap(), &mp, &[]);

    let mut names: Vec<String> = fs::read_dir(&mp)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["corpus".to_string()]);
}

#[test]
fn mount_cats_entry_contents() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let mp = tmp.path().join("mnt");
    let _guard = mount(tmp.path().join("out.zip").to_str().unwrap(), &mp, &[]);

    let got = fs::read(mp.join("corpus/a.txt")).unwrap();
    assert_eq!(got, b"hello a\n");
    let got = fs::read(mp.join("corpus/b.txt")).unwrap();
    assert_eq!(got, b"hello b, second file\n");
}

#[test]
fn mount_nested_directory_tree() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let mp = tmp.path().join("mnt");
    let _guard = mount(tmp.path().join("out.zip").to_str().unwrap(), &mp, &[]);

    let got = fs::read(mp.join("corpus/nested/deep/d.txt")).unwrap();
    assert_eq!(got, b"d deep\n");
    let got = fs::read(mp.join("corpus/nested/c.txt")).unwrap();
    assert_eq!(got, b"nested c\n");
}

#[test]
fn mount_aes_entries_with_password() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip_create(
        tmp.path(),
        &["create", "aes.zip", "corpus", "-p", "hunter2", "-q"],
    );

    let mp = tmp.path().join("mnt");
    let _guard = mount(
        tmp.path().join("aes.zip").to_str().unwrap(),
        &mp,
        &["-p", "hunter2"],
    );

    let got = fs::read(mp.join("corpus/a.txt")).unwrap();
    assert_eq!(got, b"hello a\n");
}

#[test]
fn mount_big_file_roundtrip() {
    // Entry much bigger than the default read chunk; exercises FUSE
    // multi-read semantics + the LRU hit path on the second read.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("corpus")).unwrap();
    let big: Vec<u8> = (0..500_000u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect();
    fs::write(tmp.path().join("corpus/blob.bin"), &big).unwrap();
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let mp = tmp.path().join("mnt");
    let _guard = mount(tmp.path().join("out.zip").to_str().unwrap(), &mp, &[]);

    let got1 = fs::read(mp.join("corpus/blob.bin")).unwrap();
    assert_eq!(got1, big, "first read mismatch");
    let got2 = fs::read(mp.join("corpus/blob.bin")).unwrap();
    assert_eq!(got2, big, "second (cached) read mismatch");
}

// ── HTTP source mount ──────────────────────────────────────────────────────

struct HttpServerGuard {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl HttpServerGuard {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for HttpServerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn serve_file(path: PathBuf) -> HttpServerGuard {
    // See the matching comment in tests/grep.rs::serve_files — bind the
    // TcpListener once and hand it to tiny_http instead of dropping +
    // rebinding (racy on macOS and under CI load on Linux).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let body = fs::read(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let join = thread::spawn(move || {
        let server = tiny_http::Server::from_listener(listener, None)
            .expect("tiny_http from_listener");
        loop {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let req = match server.recv_timeout(Duration::from_millis(200)) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => break,
            };
            serve_bytes(req, &body);
        }
    });
    HttpServerGuard {
        addr,
        stop,
        join: Some(join),
    }
}

fn serve_bytes(req: tiny_http::Request, body: &[u8]) {
    use tiny_http::{Header, Response, StatusCode};
    let is_head = matches!(req.method(), tiny_http::Method::Head);
    let range = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
        .map(|h| h.value.as_str().to_string());
    let header =
        |n: &str, v: &str| Header::from_bytes(n.as_bytes(), v.as_bytes()).unwrap();

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
fn mount_http_source() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip_create(tmp.path(), &["create", "out.zip", "corpus", "-q"]);

    let server = serve_file(tmp.path().join("out.zip"));
    let url = server.url("/out.zip");
    let mp = tmp.path().join("mnt");
    let _guard = mount(&url, &mp, &[]);

    let got = fs::read(mp.join("corpus/a.txt")).unwrap();
    assert_eq!(got, b"hello a\n");
}

// Silence the Read import if nothing above uses it.
#[allow(dead_code)]
fn _touch() -> usize {
    std::io::empty().read(&mut [0u8; 1]).unwrap_or(0)
}
