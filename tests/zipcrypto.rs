//! End-to-end `tzip cat` / `tzip x` against real ZipCrypto-encrypted
//! archives produced by Info-ZIP's `zip -e`. Gated on `have_zip_cmd()`
//! so CI environments without Info-ZIP skip cleanly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run_tzip_capture(cwd: &Path, args: &[&str]) -> (bool, String, String) {
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

fn tzip_cat_bytes(cwd: &Path, args: &[&str]) -> (bool, Vec<u8>, String) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run tzip cat");
    (
        out.status.success(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn have_zip_cmd() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("command -v zip >/dev/null 2>&1")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a ZipCrypto-encrypted zip: `zip -e -P <pw> out.zip <files>`.
/// Info-ZIP accepts the password non-interactively via `-P`; the zip
/// uses method 0 or 8 with GP bit 0 set, which is exactly the
/// ZipCrypto format tzip now decrypts.
fn make_zipcrypto_archive(cwd: &Path, out: &str, pw: &str, inputs: &[&str]) {
    let mut args = vec!["-q", "-e", "-P", pw, out];
    args.extend_from_slice(inputs);
    let st = Command::new("zip")
        .current_dir(cwd)
        .args(&args)
        .status()
        .expect("spawn zip -e");
    assert!(st.success(), "zip -e failed for {}", out);
}

#[test]
fn cat_zipcrypto_deflate_entry() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let payload = "attack at dawn\nfallback at dusk\n";
    fs::write(tmp.path().join("secret.txt"), payload.as_bytes()).unwrap();
    make_zipcrypto_archive(tmp.path(), "zc.zip", "hunter2", &["secret.txt"]);

    let (ok, stdout, stderr) =
        tzip_cat_bytes(tmp.path(), &["cat", "zc.zip", "secret.txt", "-p", "hunter2"]);
    assert!(ok, "cat failed: {stderr}");
    assert_eq!(stdout, payload.as_bytes());
}

#[test]
fn cat_zipcrypto_wrong_password_errors() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let plaintext: &[u8] = b"doesn't matter\n";
    fs::write(tmp.path().join("secret.txt"), plaintext).unwrap();
    make_zipcrypto_archive(tmp.path(), "zc.zip", "correct", &["secret.txt"]);

    // ZipCrypto's password check compares a single byte (the high byte
    // of the CRC-32) against the last byte of the decrypted
    // encryption header — so a wrong password happens to pass the
    // check with probability ~1/256. Firing four distinct wrong
    // passwords drops the all-pass probability to (1/256)^4 ≈ 1/4 B,
    // effectively zero. We require at least one explicit rejection
    // AND assert that the "accepted" ones (false-positive check)
    // never produced the actual plaintext.
    let wrong_passwords = ["wrong", "noway", "bad-pw", "sesame"];
    let mut any_rejected = false;
    for pw in &wrong_passwords {
        let (ok, stdout, stderr) =
            tzip_cat_bytes(tmp.path(), &["cat", "zc.zip", "secret.txt", "-p", pw]);
        if !ok {
            any_rejected = true;
            assert!(
                stderr.contains("ZipCrypto password check failed")
                    || stderr.contains("password")
                    || stderr.contains("crc"),
                "unhelpful error for wrong password {:?}: {}",
                pw,
                stderr
            );
        } else {
            // False-positive check: the stream cipher decrypted the
            // body with a bogus key, so any bytes written to stdout
            // are garbage. They must not match the real plaintext.
            assert_ne!(
                stdout, plaintext,
                "wrong password {:?} produced the correct plaintext — would be a crypto bug",
                pw,
            );
        }
    }
    assert!(
        any_rejected,
        "all four wrong passwords passed ZipCrypto's one-byte check — unlucky run, re-run; or a real regression"
    );
}

#[test]
fn extract_zipcrypto_multiple_entries() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("a.txt", b"alpha body\n" as &[u8]),
        ("b.txt", b"beta body text\n"),
        ("c.bin", b"\x00\x01\x02binary\x7f\x80\xff"),
    ] {
        fs::write(tmp.path().join(name), body).unwrap();
    }
    make_zipcrypto_archive(
        tmp.path(),
        "zc.zip",
        "pw",
        &["a.txt", "b.txt", "c.bin"],
    );

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    let (ok, _stdout, stderr) = run_tzip_capture(
        tmp.path(),
        &["x", "zc.zip", "--to", out_dir.to_str().unwrap(), "-p", "pw"],
    );
    assert!(ok, "extract failed: {stderr}");
    assert_eq!(fs::read(out_dir.join("a.txt")).unwrap(), b"alpha body\n");
    assert_eq!(fs::read(out_dir.join("b.txt")).unwrap(), b"beta body text\n");
    assert_eq!(
        fs::read(out_dir.join("c.bin")).unwrap(),
        b"\x00\x01\x02binary\x7f\x80\xff"
    );
}

#[test]
fn ls_marks_zipcrypto_as_encrypted() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("plain.txt"), b"nothing special\n").unwrap();
    make_zipcrypto_archive(tmp.path(), "zc.zip", "pw", &["plain.txt"]);

    let (ok, stdout, _) = run_tzip_capture(tmp.path(), &["ls", "zc.zip"]);
    assert!(ok);
    // ls's encryption marker is an asterisk column; the plain.txt row
    // should carry one.
    let row = stdout
        .lines()
        .find(|l| l.contains("plain.txt"))
        .expect("plain.txt row missing in ls");
    assert!(
        row.contains('*'),
        "expected encryption marker on row: {row}"
    );
}
