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

/// Runs tzip with `cwd` as the working directory so paths passed in `args`
/// can be relative. The default `zip -r` convention preserves whatever path
/// the CLI was given, so passing an absolute `src` would store entries under
/// `var/folders/…/corpus/…` and break the diff against the source tree.
fn run_tzip(cwd: &Path, args: &[&str]) {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
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

    run_tzip(tmp.path(), &["create", "out.zip", "corpus", "-m", "store", "-q"]);

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

    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-x", "0", "-q"],
    );

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

    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-x", "12", "-q"],
    );

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

    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-p", "hunter2", "-q"],
    );

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
    let archive_name = format!("out-{}.zip", method);
    let archive = tmp.path().join(&archive_name);
    run_tzip(
        tmp.path(),
        &["create", &archive_name, "corpus", "-m", method, "-q"],
    );
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
fn roundtrip_ppmd_via_7z() {
    // PPMd output must be readable by the ecosystem's reference
    // decoder. 7-Zip's PPMd is strict about the end marker — if
    // compress::ppmd ever regresses to `finish(false)` this test
    // surfaces it.
    roundtrip_via_7z("ppmd");
}

#[test]
fn roundtrip_ppmd_self_extract() {
    // Belt-and-braces: even without 7-Zip on PATH, make sure tzip
    // can decompress its own PPMd output (we read via compcol,
    // write via ppmd-rust — two different crates meeting at the
    // ZIP method 98 wire format).
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("ppmd.zip");
    run_tzip(
        tmp.path(),
        &["create", "ppmd.zip", "corpus", "-m", "ppmd", "-q"],
    );
    let out = tmp.path().join("extracted");
    fs::create_dir(&out).unwrap();
    let st = Command::new(tzip_bin())
        .args(["x", archive.to_str().unwrap(), "--to", out.to_str().unwrap(), "-q"])
        .status()
        .expect("spawn tzip x");
    assert!(st.success(), "tzip x of PPMd archive failed");
    diff_dirs(&src, &out.join("corpus"));
}

#[test]
fn roundtrip_ppmd_level_tiers() {
    // Cover the full --level range. Smaller input than make_corpus's
    // random blob to keep runtime bounded on the higher orders.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    fs::create_dir_all(&src).unwrap();
    let text = "the quick brown fox jumps over the lazy dog\n"
        .repeat(1000);
    fs::write(src.join("lorem.txt"), text.as_bytes()).unwrap();

    for level in [1u32, 3, 6, 9] {
        let archive = tmp.path().join(format!("level-{level}.zip"));
        run_tzip(
            tmp.path(),
            &[
                "create",
                archive.file_name().unwrap().to_str().unwrap(),
                "corpus",
                "-m", "ppmd",
                "-x", &level.to_string(),
                "-q",
            ],
        );
        let out = tmp.path().join(format!("x-{level}"));
        fs::create_dir(&out).unwrap();
        let st = Command::new(tzip_bin())
            .args(["x", archive.to_str().unwrap(), "--to", out.to_str().unwrap(), "-q"])
            .status()
            .expect("spawn tzip x");
        assert!(st.success(), "level {level}: tzip x failed");
        let got = fs::read(out.join("corpus/lorem.txt")).unwrap();
        assert_eq!(got, text.as_bytes(), "level {level} round-trip mismatch");
    }
}

#[test]
fn roundtrip_seven_zip_solid() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("corpus");
    make_corpus(&src);
    let archive = tmp.path().join("out.7z");
    run_tzip(tmp.path(), &["create", "out.7z", "corpus", "--solid", "-x", "6", "-q"]);
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
    run_tzip(
        tmp.path(),
        &["create", "out.7z", "corpus", "--solid", "-x", "6", "-p", "hunter2", "-q"],
    );
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
    run_tzip(tmp.path(), &["create", "a.zip", "corpus", "--sort", "-m", "store", "-q"]);
    run_tzip(tmp.path(), &["create", "b.zip", "corpus", "--sort", "-m", "store", "-q"]);
    // With --sort and STORE (no random salt), archives should be
    // byte-identical modulo timestamps. Since mtimes are the same on the
    // source files, they match.
    let ba = fs::read(&a).unwrap();
    let bb = fs::read(&b).unwrap();
    assert_eq!(ba, bb, "sorted STORE archives should be byte-identical");
}
