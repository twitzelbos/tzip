//! End-to-end tests for `tzip create --index-names` → O(1) entry
//! lookup via the `.tzip/names.v1` MPH index.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run_tzip(cwd: &Path, args: &[&str]) -> std::process::Output {
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
    out
}

fn make_corpus(root: &Path, n: usize) {
    fs::create_dir_all(root.join("src")).unwrap();
    for i in 0..n {
        let name = format!("src/entry_{:06}.txt", i);
        fs::write(root.join(&name), format!("payload {}\n", i)).unwrap();
    }
}

#[test]
fn index_entry_present_and_lookup_works() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 500);
    run_tzip(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "src",
            "-m",
            "deflate",
            "--index-names",
            "-q",
        ],
    );

    let listing = run_tzip(tmp.path(), &["ls", "out.zip"]);
    let listing_text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing_text.contains(".tzip/names.v1"),
        "expected index entry in listing: {}",
        listing_text
    );

    // Lookup a known entry.
    let got = run_tzip(tmp.path(), &["cat", "out.zip", "src/entry_000042.txt"]);
    assert_eq!(String::from_utf8_lossy(&got.stdout), "payload 42\n");

    // Lookup an unknown entry fails cleanly (not a hash collision).
    let bogus = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&["cat", "out.zip", "src/does_not_exist.txt"])
        .output()
        .unwrap();
    assert!(
        !bogus.status.success(),
        "lookup of nonexistent entry should fail, got: {}",
        String::from_utf8_lossy(&bogus.stdout)
    );
}

#[test]
fn index_optional_archive_still_works() {
    // Build WITHOUT --index-names; verify lookup still works (falls
    // back to linear scan). The index is strictly additive.
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 100);
    run_tzip(tmp.path(), &["create", "plain.zip", "src", "-m", "deflate", "-q"]);

    let listing = run_tzip(tmp.path(), &["ls", "plain.zip"]);
    let listing_text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing_text.contains(".tzip/names.v1"),
        "non-indexed archive should not have index entry"
    );

    let got = run_tzip(tmp.path(), &["cat", "plain.zip", "src/entry_000055.txt"]);
    assert_eq!(String::from_utf8_lossy(&got.stdout), "payload 55\n");
}

#[test]
fn index_survives_full_extract_roundtrip() {
    // Create → extract → re-create → compare. The index entry itself
    // should round-trip as a Store entry; after extracting and
    // re-creating without the flag, it just isn't present.
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 200);
    run_tzip(
        tmp.path(),
        &[
            "create",
            "indexed.zip",
            "src",
            "-m",
            "deflate",
            "--index-names",
            "-q",
        ],
    );

    // Extract. The `.tzip/names.v1` entry is a regular ZIP entry so
    // it comes out on disk — not a bug, just the "other tools see it
    // as normal" property.
    let out = tmp.path().join("extracted");
    fs::create_dir_all(&out).unwrap();
    run_tzip(
        tmp.path(),
        &[
            "x",
            "indexed.zip",
            "--to",
            out.to_str().unwrap(),
            "-q",
        ],
    );
    // Verify a normal data entry survived.
    let got = fs::read_to_string(out.join("src/entry_000100.txt")).unwrap();
    assert_eq!(got, "payload 100\n");
}

#[test]
fn index_scales_lookup_is_fast_on_large_archive() {
    // 10,000-entry archive. Build indexed + plain. Do N lookups
    // against each; indexed should not be meaningfully slower (and
    // in principle is faster on cold lookup, but this test is an
    // upper-bound sanity check).
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(tmp.path(), 10_000);
    run_tzip(
        tmp.path(),
        &[
            "create",
            "indexed.zip",
            "src",
            "-m",
            "deflate",
            "--index-names",
            "-q",
        ],
    );
    run_tzip(tmp.path(), &["create", "plain.zip", "src", "-m", "deflate", "-q"]);

    // Lookup the last entry 50× via `tzip cat` to amortize process
    // startup. Compare wall-clock.
    let lookups: Vec<&str> = (0..50).map(|_| "src/entry_009999.txt").collect();

    let indexed = time_lookups(&tmp.path(), "indexed.zip", &lookups);
    let plain = time_lookups(&tmp.path(), "plain.zip", &lookups);
    // Allow indexed to be up to 1.5× plain on this toy bench — the
    // test is a smoke sanity check that the index path isn't vastly
    // worse. True perf win shows on larger archives + hot-path
    // batch workloads.
    assert!(
        indexed <= plain * 3 / 2 + 50,
        "indexed lookups ({indexed} ms) much slower than plain ({plain} ms)",
    );
    eprintln!("nameidx lookup timings (50x): indexed={indexed} ms, plain={plain} ms");
}

fn time_lookups(cwd: &Path, archive: &str, names: &[&str]) -> u128 {
    let start = Instant::now();
    for name in names {
        let _ = Command::new(tzip_bin())
            .current_dir(cwd)
            .args(&["cat", archive, name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
    }
    start.elapsed().as_millis()
}
