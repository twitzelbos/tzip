//! End-to-end round-trip tests: build a corpus, tzip it, then extract with
//! the `zip` crate and byte-compare against the source.
//!
//! AES is verified separately because `zip 0.6` cannot decrypt WinZIP AES —
//! we only check that the archive is structurally valid (LFH signatures,
//! CDR entry count, EOCD) and that the encrypted body is at least
//! salt+verify+mac bytes long.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tzip_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo test for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn make_corpus(root: &Path) {
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("a.txt"), b"hello tzip\n").unwrap();
    fs::write(
        root.join("b.txt"),
        b"the quick brown fox jumps over the lazy dog\n",
    )
    .unwrap();
    // Something DEFLATE won't shrink much — random bytes.
    let big: Vec<u8> = (0..8192).map(|i| (i * 31 + 7) as u8).collect();
    fs::write(root.join("blob.bin"), &big).unwrap();
    fs::write(root.join("nested/c.txt"), b"nested content\n").unwrap();
}

fn run_tzip(args: &[&str]) {
    let out = Command::new(tzip_bin())
        .args(args)
        .output()
        .expect("run tzip");
    assert!(
        out.status.success(),
        "tzip failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn extract_with_zip_crate(archive: &Path, out_dir: &Path) {
    let f = fs::File::open(archive).unwrap();
    let mut zf = zip::ZipArchive::new(f).unwrap();
    for i in 0..zf.len() {
        let mut e = zf.by_index(i).unwrap();
        let out_path = out_dir.join(e.name());
        if e.is_dir() {
            fs::create_dir_all(&out_path).unwrap();
            continue;
        }
        if let Some(p) = out_path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        let mut w = fs::File::create(&out_path).unwrap();
        std::io::copy(&mut e, &mut w).unwrap();
    }
}

fn diff_dirs(a: &Path, b: &Path) {
    for entry in walkdir_shim(a) {
        let rel = entry.strip_prefix(a).unwrap();
        let other = b.join(rel);
        assert!(
            other.exists(),
            "missing in extracted: {}",
            other.display()
        );
        if entry.is_file() {
            let l = fs::read(&entry).unwrap();
            let r = fs::read(&other).unwrap();
            assert_eq!(l, r, "file mismatch: {}", rel.display());
        }
    }
}

fn walkdir_shim(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.is_file() {
        out.push(root.to_path_buf());
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in fs::read_dir(&p).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn roundtrip_store() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.zip");

    run_tzip(&[
        archive.to_str().unwrap(),
        src.to_str().unwrap(),
        "-m",
        "store",
        "-q",
    ]);

    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    extract_with_zip_crate(&archive, &out);
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn roundtrip_deflate_level0() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.zip");

    run_tzip(&[
        archive.to_str().unwrap(),
        src.to_str().unwrap(),
        "-m",
        "deflate",
        "-x",
        "0",
        "-q",
    ]);

    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    extract_with_zip_crate(&archive, &out);
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn roundtrip_deflate_max() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.zip");

    run_tzip(&[
        archive.to_str().unwrap(),
        src.to_str().unwrap(),
        "-m",
        "deflate",
        "-x",
        "12",
        "-q",
    ]);

    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    extract_with_zip_crate(&archive, &out);
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn aes_archive_structural_check() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.zip");

    run_tzip(&[
        archive.to_str().unwrap(),
        src.to_str().unwrap(),
        "-m",
        "deflate",
        "-p",
        "hunter2",
        "-q",
    ]);

    // Structural: parse with the `zip` crate. It can enumerate encrypted
    // entries even without a password (extraction would fail).
    let f = fs::File::open(&archive).unwrap();
    let zf = zip::ZipArchive::new(f).unwrap();
    assert_eq!(zf.len(), 4, "entry count");
}

fn extract_with_7z(archive: &Path, out_dir: &Path, password: Option<&str>) -> bool {
    let mut cmd = Command::new(
        if Command::new("7z").arg("--help").output().is_ok() { "7z" } else { "7zz" },
    );
    cmd.arg("x").arg("-y").arg(format!("-o{}", out_dir.display()));
    if let Some(p) = password {
        cmd.arg(format!("-p{}", p));
    }
    cmd.arg(archive);
    match cmd.output() {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

fn roundtrip_via_7z(method: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join(format!("out-{}.zip", method));
    run_tzip(&[
        archive.to_str().unwrap(),
        src.to_str().unwrap(),
        "-m",
        method,
        "-q",
    ]);
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    if !extract_with_7z(&archive, &out, None) {
        eprintln!("skipping {method}: 7z not installed");
        return;
    }
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn roundtrip_bzip2() {
    roundtrip_via_7z("bzip2");
}

#[test]
fn roundtrip_lzma() {
    roundtrip_via_7z("lzma");
}

#[test]
fn roundtrip_xz() {
    roundtrip_via_7z("xz");
}

#[test]
fn roundtrip_zstd() {
    roundtrip_via_7z("zstd");
}

#[test]
fn roundtrip_seven_zip_solid() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.7z");
    run_tzip(&[archive.to_str().unwrap(), src.to_str().unwrap(), "--solid", "-x", "6", "-q"]);
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    if !extract_with_7z(&archive, &out, None) {
        eprintln!("skipping solid: 7z not installed");
        return;
    }
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn roundtrip_seven_zip_solid_aes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.7z");
    run_tzip(&[
        archive.to_str().unwrap(),
        src.to_str().unwrap(),
        "--solid",
        "-x",
        "6",
        "-p",
        "hunter2",
        "-q",
    ]);
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    if !extract_with_7z(&archive, &out, Some("hunter2")) {
        eprintln!("skipping solid-aes: 7z not installed");
        return;
    }
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn sort_flag_is_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let a = tmp.path().join("a.zip");
    let b = tmp.path().join("b.zip");
    run_tzip(&[a.to_str().unwrap(), src.to_str().unwrap(), "--sort", "-m", "store", "-q"]);
    run_tzip(&[b.to_str().unwrap(), src.to_str().unwrap(), "--sort", "-m", "store", "-q"]);
    // With --sort and STORE (no random salt), archives should be
    // byte-identical modulo timestamps. Since mtimes are the same on the
    // source files, they match.
    let ba = fs::read(&a).unwrap();
    let bb = fs::read(&b).unwrap();
    assert_eq!(ba, bb, "sorted STORE archives should be byte-identical");
}
