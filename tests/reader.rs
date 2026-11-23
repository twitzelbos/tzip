//! Reader-stack end-to-end tests: `tzip ls`, `tzip cat`, `tzip x`.
//!
//! Same conventions as `tests/roundtrip.rs` / `tests/update.rs`: tzip is
//! launched as a subprocess with the tempdir as its cwd so relative
//! paths resolve correctly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// `true` if the system `zip` command is on PATH. Some CI environments
/// omit it; tests that use it skip rather than fail when it's missing.
fn have_zip_cmd() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("command -v zip >/dev/null 2>&1")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `tzip cat` and capture the raw stdout bytes (don't lossy-convert).
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

// ── ls ─────────────────────────────────────────────────────────────────────

#[test]
fn ls_lists_all_entries() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), b"bb\n").unwrap();
    fs::write(tmp.path().join("c.txt"), b"ccc\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "c.txt", "-q"]);

    let (ok, stdout, _) = run_tzip_capture(tmp.path(), &["ls", "out.zip"]);
    assert!(ok, "ls failed");
    for name in &["a.txt", "b.txt", "c.txt"] {
        assert!(stdout.contains(name), "ls output missing {name}:\n{stdout}");
    }
    assert!(stdout.contains("3 files"), "footer missing count");
}

#[test]
fn ls_marks_encrypted_entries() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("secret.txt"), b"shh\n").unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "secret.txt", "-p", "pw", "-q"],
    );

    let (ok, stdout, _) = run_tzip_capture(tmp.path(), &["ls", "out.zip"]);
    assert!(ok);
    // Locate the row for secret.txt and confirm the encrypted marker (`*`)
    // appears in the column just before the name.
    let row = stdout
        .lines()
        .find(|l| l.contains("secret.txt"))
        .expect("secret.txt row not found");
    // Strip the trailing name; there should be a `*` before it.
    let before_name = row.rsplit_once("secret.txt").unwrap().0;
    assert!(
        before_name.trim_end().ends_with('*'),
        "expected encrypted marker before name in row: {row:?}"
    );
}

// ── cat, per method ───────────────────────────────────────────────────────

fn cat_roundtrip_for_method(method: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let want = b"The quick brown fox jumps over the lazy dog.\n".repeat(100);
    fs::write(tmp.path().join("data.txt"), &want).unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "data.txt", "-m", method, "-q"],
    );

    let (ok, stdout, stderr) = tzip_cat_bytes(tmp.path(), &["cat", "out.zip", "data.txt"]);
    assert!(ok, "cat failed for {method}: {stderr}");
    assert_eq!(stdout, want, "cat roundtrip mismatch for {method}");
}

#[test]
fn cat_store() {
    cat_roundtrip_for_method("store");
}

#[test]
fn cat_deflate() {
    cat_roundtrip_for_method("deflate");
}

#[test]
fn cat_bzip2() {
    cat_roundtrip_for_method("bzip2");
}

#[test]
fn cat_lzma() {
    cat_roundtrip_for_method("lzma");
}

#[test]
fn cat_xz() {
    cat_roundtrip_for_method("xz");
}

#[test]
fn cat_zstd() {
    cat_roundtrip_for_method("zstd");
}

// ── AES cat ───────────────────────────────────────────────────────────────

#[test]
fn cat_aes_deflate_with_password() {
    let tmp = tempfile::tempdir().unwrap();
    let want = b"encrypted body\n".repeat(50);
    fs::write(tmp.path().join("secret.txt"), &want).unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "secret.txt", "-p", "hunter2", "-q"],
    );
    let (ok, stdout, stderr) = tzip_cat_bytes(
        tmp.path(),
        &["cat", "out.zip", "secret.txt", "-p", "hunter2"],
    );
    assert!(ok, "cat failed: {stderr}");
    assert_eq!(stdout, want);
}

#[test]
fn cat_aes_store_with_password() {
    let tmp = tempfile::tempdir().unwrap();
    let want = b"stored but encrypted\n".to_vec();
    fs::write(tmp.path().join("secret.txt"), &want).unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "secret.txt", "-m", "store", "-p", "pw", "-q"],
    );
    let (ok, stdout, _) = tzip_cat_bytes(tmp.path(), &["cat", "out.zip", "secret.txt", "-p", "pw"]);
    assert!(ok);
    assert_eq!(stdout, want);
}

#[test]
fn cat_aes_missing_password_errors() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("secret.txt"), b"hi\n").unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "secret.txt", "-p", "pw", "-q"],
    );
    let (ok, _stdout, stderr) = tzip_cat_bytes(tmp.path(), &["cat", "out.zip", "secret.txt"]);
    assert!(!ok, "expected failure on missing password");
    assert!(
        stderr.to_lowercase().contains("encrypted")
            || stderr.to_lowercase().contains("password"),
        "missing-password error should mention it; got: {stderr}"
    );
}

#[test]
fn cat_aes_wrong_password_errors() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("secret.txt"), b"hi\n").unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "secret.txt", "-p", "right", "-q"],
    );
    let (ok, _stdout, stderr) = tzip_cat_bytes(
        tmp.path(),
        &["cat", "out.zip", "secret.txt", "-p", "wrong"],
    );
    assert!(!ok, "expected failure on wrong password");
    assert!(
        stderr.to_lowercase().contains("wrong password")
            || stderr.to_lowercase().contains("verifier"),
        "wrong-password error should mention it; got: {stderr}"
    );
}

// ── extract ───────────────────────────────────────────────────────────────

fn write_corpus(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fs::create_dir_all(root.join("nested/deep")).unwrap();
    let files: Vec<(PathBuf, Vec<u8>)> = vec![
        (root.join("a.txt"), b"hello a\n".to_vec()),
        (
            root.join("b.txt"),
            b"the quick brown fox jumps over the lazy dog\n".to_vec(),
        ),
        (
            root.join("blob.bin"),
            (0..4096u32).map(|i| (i * 31 + 7) as u8).collect(),
        ),
        (root.join("nested/c.txt"), b"nested content\n".to_vec()),
        (
            root.join("nested/deep/d.txt"),
            b"deeper content\n".to_vec(),
        ),
    ];
    for (p, content) in &files {
        fs::write(p, content).unwrap();
    }
    files
}

fn extract_and_verify(method: &str, password: Option<&str>) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    let files = write_corpus(&src);

    let mut create_args: Vec<String> = vec![
        "create".into(),
        "out.zip".into(),
        "corpus".into(),
        "-m".into(),
        method.into(),
        "-q".into(),
    ];
    if let Some(pw) = password {
        create_args.push("-p".into());
        create_args.push(pw.into());
    }
    let create_args_refs: Vec<&str> = create_args.iter().map(|s| s.as_str()).collect();
    run_tzip(tmp.path(), &create_args_refs);

    let mut x_args: Vec<String> =
        vec!["x".into(), "out.zip".into(), "--to".into(), "extracted".into()];
    if let Some(pw) = password {
        x_args.push("-p".into());
        x_args.push(pw.into());
    }
    let x_args_refs: Vec<&str> = x_args.iter().map(|s| s.as_str()).collect();
    run_tzip(tmp.path(), &x_args_refs);

    for (p, want) in &files {
        let rel = p.strip_prefix(tmp.path()).unwrap();
        let got = fs::read(tmp.path().join("extracted").join(rel))
            .unwrap_or_else(|e| panic!("missing {} after extract: {e}", rel.display()));
        assert_eq!(&got, want, "content mismatch for {}", rel.display());
    }
}

#[test]
fn extract_whole_archive_plain() {
    extract_and_verify("deflate", None);
}

#[test]
fn extract_whole_archive_aes() {
    extract_and_verify("deflate", Some("hunter2"));
}

#[test]
fn extract_whole_archive_store_aes() {
    extract_and_verify("store", Some("pw"));
}

#[test]
fn extract_selected_entries() {
    let tmp = tempfile::tempdir().unwrap();
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(tmp.path().join(n), format!("{n}\n").as_bytes()).unwrap();
    }
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "a.txt", "b.txt", "c.txt", "-q"],
    );
    run_tzip(
        tmp.path(),
        &[
            "x", "out.zip", "a.txt", "c.txt", "--to", "extracted",
        ],
    );
    let out = tmp.path().join("extracted");
    assert!(out.join("a.txt").exists());
    assert!(!out.join("b.txt").exists());
    assert!(out.join("c.txt").exists());
}

// ── zip-slip guard ────────────────────────────────────────────────────────

#[test]
fn extract_rejects_zip_slip() {
    // Hand-fabricate an archive whose CDR entry name contains `..`.
    // We assemble it by starting from a tzip-created single-entry
    // archive and rewriting the LFH + CDR name bytes.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("inner"), b"payload\n").unwrap();
    run_tzip(
        tmp.path(),
        &["create", "mal.zip", "inner", "-m", "store", "-q"],
    );

    let path = tmp.path().join("mal.zip");
    let mut bytes = fs::read(&path).unwrap();
    // Replace b"inner" with b"../e~" in both the LFH and CDR name copies.
    // Same length (5 bytes), so no offsets shift.
    let needle: &[u8] = b"inner";
    let replacement: &[u8] = b"../e~";
    assert_eq!(needle.len(), replacement.len());
    let mut hits = 0;
    for i in 0..bytes.len() - needle.len() {
        if &bytes[i..i + needle.len()] == needle {
            bytes[i..i + needle.len()].copy_from_slice(replacement);
            hits += 1;
        }
    }
    assert!(hits >= 2, "expected to find name in LFH + CDR, found {hits}");
    fs::write(&path, &bytes).unwrap();

    let (ok, _stdout, stderr) =
        run_tzip_capture(tmp.path(), &["x", "mal.zip", "--to", "extracted"]);
    assert!(!ok, "extract should refuse zip-slip entry");
    assert!(
        stderr.contains("..") || stderr.to_lowercase().contains("unsafe"),
        "expected a zip-slip error, got: {stderr}"
    );
}

// ── identity: create then x ───────────────────────────────────────────────

#[test]
fn create_then_extract_is_identity_plain() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    let files = write_corpus(&src);
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-q"],
    );
    run_tzip(
        tmp.path(),
        &["x", "out.zip", "--to", "extracted"],
    );
    for (p, want) in &files {
        let rel = p.strip_prefix(tmp.path()).unwrap();
        let got = fs::read(tmp.path().join("extracted").join(rel)).unwrap();
        assert_eq!(&got, want);
    }
}

// ── Comments + external_attr (PR #19 scope) ────────────────────────────────

#[test]
fn ls_shows_entry_and_archive_comments() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("hello.txt"), b"hi\n").unwrap();

    // `zip -c` is interactive; use a heredoc via sh to script the
    // per-entry and archive comments.
    let script = r#"
        set -eu
        cd "$1"
        {
            echo "per-entry note"
            echo ""
        } | zip -q -c out.zip hello.txt
        echo "top-level archive banner" | zip -q -z out.zip
    "#;
    let st = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(tmp.path())
        .status()
        .expect("spawn sh");
    assert!(st.success(), "zip comment script failed");

    // Without -v: the entry comment line is suppressed; archive banner
    // still shows after the summary.
    let (ok, stdout, _) = run_tzip_capture(tmp.path(), &["ls", "out.zip"]);
    assert!(ok);
    assert!(
        !stdout.contains("per-entry note"),
        "non-verbose ls should hide entry comments: {stdout}"
    );
    assert!(
        stdout.contains("top-level archive banner"),
        "archive comment missing: {stdout}"
    );

    // -v: the entry comment appears below its row.
    let (ok, stdout, _) = run_tzip_capture(tmp.path(), &["ls", "out.zip", "-v"]);
    assert!(ok);
    assert!(stdout.contains("per-entry note"), "verbose ls missing entry comment: {stdout}");
}

#[test]
fn find_json_includes_comment_and_unix_mode() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("script.sh");
    fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Use system zip so external_attr carries real Unix mode bits.
    let st = Command::new("zip")
        .current_dir(tmp.path())
        .args(["-q", "out.zip", "script.sh"])
        .status()
        .expect("spawn zip");
    assert!(st.success(), "zip failed");

    let (ok, stdout, _) = run_tzip_capture(
        tmp.path(),
        &["find", "out.zip", "--json"],
    );
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap())
        .expect("valid JSON");
    assert_eq!(v["entry"], "script.sh");
    #[cfg(unix)]
    {
        let mode = v["unix_mode"].as_u64().expect("unix_mode present on Unix-origin zip");
        // Permission bits only (type bits already stripped).
        assert_eq!(mode & 0o777, 0o755, "expected 0o755, got {:o}", mode);
    }
}

#[cfg(unix)]
#[test]
fn extract_restores_unix_mode_when_origin_is_unix() {
    if !have_zip_cmd() {
        eprintln!("skipping: system `zip` command not available");
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("exec.sh");
    fs::write(&src, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
    let st = Command::new("zip")
        .current_dir(tmp.path())
        .args(["-q", "out.zip", "exec.sh"])
        .status()
        .expect("spawn zip");
    assert!(st.success());

    let out_dir = tmp.path().join("extracted");
    fs::create_dir_all(&out_dir).unwrap();
    run_tzip(
        tmp.path(),
        &["x", "out.zip", "--to", out_dir.to_str().unwrap()],
    );

    let got = fs::metadata(out_dir.join("exec.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(got, 0o750, "expected 0o750, got {:o}", got);
}
