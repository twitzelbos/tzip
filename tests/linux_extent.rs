//! Linux extent-order (`--raw-block`) reader regression tests.
//!
//! The core guarantee: changing the *read* order must never change archive
//! bytes. We build a corpus, archive it with the default reader and with
//! `--raw-block` (both under `--sort` for a deterministic order), and require
//! the two archives to be byte-identical. This holds on any filesystem — on
//! one without FIEMAP support (e.g. the tmpfs `/tmp` these tests usually run
//! on) the reader simply falls back to walk order, which must still produce
//! identical bytes.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn make_corpus(root: &Path) {
    // A spread of sizes across the mmap threshold (1 MiB), plus empties and
    // nesting, so both the mmap and read_to_end paths are exercised.
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::write(root.join("empty.bin"), b"").unwrap();
    fs::write(root.join("a/b/emptynested.bin"), b"").unwrap();
    fs::write(root.join("tiny.txt"), b"hello tzip\n").unwrap();
    for (name, len) in [
        ("small.bin", 4096usize),
        ("mid.bin", 300_000),
        ("big.bin", 3_000_000),
        ("a/b/c/deep.bin", 5_000_000),
    ] {
        let data: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8).collect();
        fs::write(root.join(name), &data).unwrap();
    }
}

fn run(args: &[&str]) {
    let out = Command::new(tzip_bin()).args(args).output().expect("run tzip");
    assert!(
        out.status.success(),
        "tzip {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tmp(label: &str) -> PathBuf {
    // Unique per test — cargo runs test fns as parallel threads in one
    // process, so keying on pid alone would let two tests share a dir.
    let d = std::env::temp_dir().join(format!("tzip_ext_{}_{label}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn raw_block_sorted_is_byte_identical() {
    let d = tmp("sorted");
    let src = d.join("src");
    make_corpus(&src);

    let def = d.join("default.zip");
    let raw = d.join("raw.zip");
    let src_s = src.to_str().unwrap();
    run(&["--sort", "-q", def.to_str().unwrap(), src_s]);
    run(&["--sort", "-q", "--raw-block=true", raw.to_str().unwrap(), src_s]);

    let a = fs::read(&def).unwrap();
    let b = fs::read(&raw).unwrap();
    assert_eq!(a, b, "--raw-block archive differs from default (sorted)");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn raw_block_default_order_same_contents() {
    // Without --sort the write order is arrival order, which can differ, but
    // the *set* of entries and their sizes must match exactly.
    let d = tmp("unsorted");
    let src = d.join("src");
    make_corpus(&src);

    let def = d.join("default.zip");
    let raw = d.join("raw.zip");
    let src_s = src.to_str().unwrap();
    run(&["-q", def.to_str().unwrap(), src_s]);
    run(&["-q", "--raw-block=true", raw.to_str().unwrap(), src_s]);

    // Same total archive size is a cheap, strong signal that the same bytes
    // were compressed (deflate is deterministic per input).
    let da = fs::metadata(&def).unwrap().len();
    let ra = fs::metadata(&raw).unwrap().len();
    assert_eq!(da, ra, "archive sizes differ between readers");

    let _ = fs::remove_dir_all(&d);
}

#[cfg(feature = "io-uring")]
#[test]
fn io_uring_sorted_is_byte_identical() {
    let d = tmp("uring");
    let src = d.join("src");
    make_corpus(&src);

    let def = d.join("default.zip");
    let uring = d.join("uring.zip");
    let src_s = src.to_str().unwrap();
    run(&["--sort", "-q", def.to_str().unwrap(), src_s]);
    run(&[
        "--sort",
        "-q",
        "--raw-block=true",
        "--io-uring",
        uring.to_str().unwrap(),
        src_s,
    ]);

    let a = fs::read(&def).unwrap();
    let b = fs::read(&uring).unwrap();
    assert_eq!(a, b, "--io-uring archive differs from default (sorted)");

    let _ = fs::remove_dir_all(&d);
}
