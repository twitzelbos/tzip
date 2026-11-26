//! Independent interoperability: build tzip archives, read them with
//! external tools (Info-ZIP `unzip`, `7z`, Python stdlib `zipfile`),
//! and vice versa. The existing round-trip tests use tzip as both
//! writer and reader; this file is the external-ecosystem check so a
//! wire-format regression in LFH/CDR layout, the AES extra field, or
//! Zip64 trailer emission can't hide behind the one party that
//! happens to agree with itself.
//!
//! Each tool's absence is tolerated (`eprintln!` + early return) so
//! the suite stays green on hosts without the tool installed. CI
//! images on Linux and macOS carry `unzip`, `7z`, and `python3`, so
//! the skip paths only trigger on bare dev environments.
//!
//! Non-goals: compressed-method coverage for tools that can't read
//! them (Zstd on stock unzip, PPMd on stock unzip). Those get
//! partial-ecosystem notes in README; we don't fabricate green
//! tests for combinations the ecosystem actually can't open.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        "tzip {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn make_corpus(root: &Path) {
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("a.txt"), b"hello tzip\n").unwrap();
    fs::write(
        root.join("b.txt"),
        b"the quick brown fox jumps over the lazy dog\n",
    )
    .unwrap();
    // Random-looking blob so DEFLATE actually compresses.
    let big: Vec<u8> = (0..8192u32).map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8).collect();
    fs::write(root.join("blob.bin"), &big).unwrap();
    fs::write(root.join("nested/c.txt"), b"nested content\n").unwrap();
}

/// Byte-compare source corpus tree against extracted tree.
fn assert_dirs_equal(a: &Path, b: &Path) {
    let a_files = collect_files(a);
    let b_files = collect_files(b);
    assert_eq!(
        a_files, b_files,
        "file lists differ between source {:?} and extracted {:?}",
        a, b
    );
    for name in a_files {
        let ab = fs::read(a.join(&name)).unwrap();
        let bb = fs::read(b.join(&name)).unwrap();
        assert_eq!(ab, bb, "file {name:?} differs between source and extracted");
    }
}

fn collect_files(root: &Path) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    walk(root, root, &mut out);
    out
}

fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeSet<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, out);
        } else {
            let rel = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            out.insert(rel);
        }
    }
}

fn write_many_or_skip(dir: &Path, n: u32) -> std::io::Result<()> {
    for i in 0..n {
        fs::write(dir.join(format!("e{:05}.txt", i)), b"x")?;
    }
    Ok(())
}

fn tool_exists(bin: &str) -> bool {
    Command::new(bin)
        .arg("-h")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty() || !o.stdout.is_empty())
        .unwrap_or(false)
}

// ── unzip (Info-ZIP) reads tzip ────────────────────────────────────────────

fn roundtrip_via_unzip(method: &str) {
    if !tool_exists("unzip") {
        eprintln!("skipping: unzip not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", method, "-q"],
    );
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    let st = Command::new("unzip")
        .arg("-q")
        .arg("-d")
        .arg(&out)
        .arg(tmp.path().join("out.zip"))
        .status()
        .expect("spawn unzip");
    assert!(st.success(), "unzip refused tzip's {method} output");
    assert_dirs_equal(&src, &out.join("corpus"));
}

#[test]
fn unzip_reads_tzip_store() {
    roundtrip_via_unzip("store");
}

#[test]
fn unzip_reads_tzip_deflate() {
    roundtrip_via_unzip("deflate");
}

// bzip2 + unzip intentionally not tested here: Apple's bundled
// `unzip 6.00 of 20 April 2009` (and many distro builds too) are
// compiled without bzip2. 7z covers bzip2 in `roundtrip.rs::roundtrip_bzip2`.

// ── 7z reads tzip AES ──────────────────────────────────────────────────────

#[test]
fn seven_zip_reads_tzip_aes_deflate() {
    let seven_zip = if tool_exists("7z") {
        "7z"
    } else if tool_exists("7zz") {
        "7zz"
    } else {
        eprintln!("skipping: 7z/7zz not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(
        tmp.path(),
        &[
            "create", "out.zip", "corpus", "-m", "deflate", "-p", "hunter2", "-q",
        ],
    );
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    let st = Command::new(seven_zip)
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", out.display()))
        .arg("-phunter2")
        .arg(tmp.path().join("out.zip"))
        .status()
        .expect("spawn 7z");
    assert!(st.success(), "7z refused tzip's AES-256 archive");
    assert_dirs_equal(&src, &out.join("corpus"));
}

#[test]
fn seven_zip_rejects_wrong_password_on_tzip_aes() {
    // The verifier byte in the AES header is 2 bytes; a wrong
    // password should fail fast at 1/65536 false-pass rate per the
    // spec. 7z's exit status + stderr "Wrong password" is the
    // ecosystem check that we emitted the verifier correctly.
    let seven_zip = if tool_exists("7z") {
        "7z"
    } else if tool_exists("7zz") {
        "7zz"
    } else {
        eprintln!("skipping: 7z/7zz not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(
        tmp.path(),
        &[
            "create", "out.zip", "corpus", "-m", "deflate", "-p", "hunter2", "-q",
        ],
    );
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    let res = Command::new(seven_zip)
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", out.display()))
        .arg("-pTOTALLY-WRONG")
        .arg(tmp.path().join("out.zip"))
        .output()
        .expect("spawn 7z");
    assert!(
        !res.status.success(),
        "7z should have rejected wrong password; stdout={} stderr={}",
        String::from_utf8_lossy(&res.stdout),
        String::from_utf8_lossy(&res.stderr),
    );
}

// ── Python stdlib zipfile reads tzip ───────────────────────────────────────

#[test]
fn python_zipfile_reads_tzip_deflate() {
    if !tool_exists("python3") {
        eprintln!("skipping: python3 not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-q"],
    );
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    // `-I` isolates the interpreter from user site packages and the
    // CWD. We only hand the paths as argv, never execute from the
    // archive's directory. Plain string literal (no format! here)
    // so Python's own `{...}` doesn't trip Rust's format parser.
    let py = r#"
import sys, zipfile, pathlib
archive = sys.argv[1]
outdir = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(archive) as z:
    z.extractall(outdir)
"#;
    let res = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(py)
        .arg(tmp.path().join("out.zip"))
        .arg(&out)
        .output()
        .expect("spawn python3");
    assert!(
        res.status.success(),
        "python3 zipfile refused tzip deflate archive: {}",
        String::from_utf8_lossy(&res.stderr),
    );
    assert_dirs_equal(&src, &out.join("corpus"));
}

// ── Info-ZIP's zip writes archives that tzip reads ─────────────────────────

#[test]
fn tzip_reads_infozip_zip_output() {
    // The reverse direction: a stock `zip` archive (what any
    // mainstream tool produces) must list and extract cleanly
    // through tzip. Guards against a reader regression where
    // tzip's LFH / CDR parsing drifts away from the canonical
    // spec layout emitted by Info-ZIP.
    if !tool_exists("zip") {
        eprintln!("skipping: zip (Info-ZIP) not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);

    let archive = tmp.path().join("infozip.zip");
    let st = Command::new("zip")
        .arg("-r")
        .arg("-q")
        .arg(&archive)
        .arg("corpus")
        .current_dir(tmp.path())
        .status()
        .expect("spawn zip");
    assert!(st.success(), "Info-ZIP zip failed");

    // tzip ls shows the entries.
    let ls = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(["ls", "infozip.zip"])
        .output()
        .expect("tzip ls");
    assert!(
        ls.status.success(),
        "tzip ls on Info-ZIP archive failed: {}",
        String::from_utf8_lossy(&ls.stderr),
    );
    let listing = String::from_utf8_lossy(&ls.stdout);
    assert!(listing.contains("corpus/a.txt"), "listing: {listing}");
    assert!(listing.contains("corpus/b.txt"), "listing: {listing}");

    // tzip x round-trips the content.
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    let st = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(["x", "infozip.zip", "--to"])
        .arg(&out)
        .status()
        .expect("tzip x");
    assert!(st.success(), "tzip x on Info-ZIP archive failed");
    assert_dirs_equal(&src, &out.join("corpus"));
}

// ── unzip reads tzip Zip64 archive ─────────────────────────────────────────

#[test]
fn unzip_reads_tzip_zip64_many_entries() {
    // Trigger the 65535-entry ZIP64 path: tzip must emit the
    // EOCD64 locator + record, and stock unzip must honour them.
    // This is the layout test that most catches wire-format drift
    // on the Zip64 path — easy to break by changing a field width.
    //
    // Use the minimum entry count that crosses the uint16 ceiling
    // (65535 → 65540) so the test still exercises the ZIP64 path
    // without stressing low-space dev machines. The archive itself
    // is small (~4 MB; 70 B / LFH × 65540).
    if !tool_exists("unzip") {
        eprintln!("skipping: unzip not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    fs::create_dir_all(&src).unwrap();
    let n: u32 = 65540;
    // Create a canary to detect ENOSPC early and skip rather than
    // fail — CI runners have plenty of space, bare dev machines
    // sometimes don't and this is an ecosystem-interop test, not
    // a free-space regression test.
    if let Err(e) = write_many_or_skip(&src, n) {
        eprintln!(
            "skipping zip64 interop: couldn't lay out {n} entries ({e})"
        );
        return;
    }
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    let st = Command::new("unzip")
        .arg("-q")
        .arg("-d")
        .arg(&out)
        .arg(tmp.path().join("out.zip"))
        .status()
        .expect("spawn unzip");
    assert!(st.success(), "unzip refused tzip's Zip64 archive");
    // Spot-check: first, boundary, last entries.
    for i in [0u32, 65534, 65535, n - 1] {
        let a = fs::read(src.join(format!("e{:05}.txt", i))).unwrap();
        let b = fs::read(out.join("corpus").join(format!("e{:05}.txt", i))).unwrap();
        assert_eq!(a, b, "Zip64 entry {i} content differs");
    }
}
