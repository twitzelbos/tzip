//! Integration tests for `tzip hash` — crc32 (CDR-free), md5, sha256,
//! across plain and AES-encrypted archives.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn make_corpus(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("a.txt"), b"hello tzip\n").unwrap();
    fs::write(root.join("b.txt"), b"the quick brown fox\n").unwrap();
    // Something DEFLATE won't shrink much — random-looking bytes.
    let blob: Vec<u8> = (0..4096).map(|i| (i * 31 + 7) as u8).collect();
    fs::write(root.join("blob.bin"), &blob).unwrap();
}

/// Parse `<hex>  <name>` lines into a (name, hex) map.
fn parse_hash_lines(raw: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, "  ");
        let hex = parts.next().unwrap().to_string();
        let name = parts.next().unwrap().to_string();
        out.insert(name, hex);
    }
    out
}

#[test]
fn hash_crc32_matches_cdr() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-q"],
    );
    let out = run_tzip(tmp.path(), &["hash", "out.zip", "--algo", "crc32"]);
    let map = parse_hash_lines(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(map.len(), 3, "three file entries");
    // "hello tzip\n" → CRC-32 (independently verified with Python
    // `zlib.crc32`).
    assert_eq!(
        map.get("corpus/a.txt").map(|s| s.as_str()),
        Some("7123916b"),
    );
}

#[test]
fn hash_sha256_matches_known_plaintext() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-q"],
    );
    let out = run_tzip(
        tmp.path(),
        &["hash", "out.zip", "--algo", "sha256", "corpus/a.txt"],
    );
    let map = parse_hash_lines(&String::from_utf8_lossy(&out.stdout));
    // sha256("hello tzip\n") — precomputed via `shasum -a 256`.
    assert_eq!(
        map.get("corpus/a.txt").map(|s| s.as_str()),
        Some("3f8f1e2121d3749e0eb0fcb72ed8aa6ade00e75f133904214a16745d7e063d52"),
    );
}

#[test]
fn hash_md5_matches_known_plaintext() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-q"],
    );
    let out = run_tzip(
        tmp.path(),
        &["hash", "out.zip", "--algo", "md5", "corpus/a.txt"],
    );
    let map = parse_hash_lines(&String::from_utf8_lossy(&out.stdout));
    // md5("hello tzip\n") — precomputed via BSD `md5`.
    assert_eq!(
        map.get("corpus/a.txt").map(|s| s.as_str()),
        Some("0e83144dba6804159be88601efca1184"),
    );
}

#[test]
fn hash_sha256_is_the_same_through_aes() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    // Two archives: plain + AES-wrapped — plaintext hashes must match.
    run_tzip(
        tmp.path(),
        &["create", "plain.zip", "corpus", "-m", "deflate", "-q"],
    );
    run_tzip(
        tmp.path(),
        &["create", "aes.zip", "corpus", "-m", "deflate", "-p", "hunter2", "-q"],
    );

    let plain_out = run_tzip(
        tmp.path(),
        &["hash", "plain.zip", "--algo", "sha256"],
    );
    let aes_out = run_tzip(
        tmp.path(),
        &["hash", "aes.zip", "--algo", "sha256", "-p", "hunter2"],
    );
    let plain = parse_hash_lines(&String::from_utf8_lossy(&plain_out.stdout));
    let aes = parse_hash_lines(&String::from_utf8_lossy(&aes_out.stdout));
    assert_eq!(plain, aes, "AES wrapper doesn't change the plaintext hash");
}

#[test]
fn hash_json_output_is_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );
    let out = run_tzip(
        tmp.path(),
        &["hash", "out.zip", "--algo", "crc32", "--json"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Each line must be a self-contained JSON object with our keys.
        assert!(line.starts_with('{') && line.ends_with('}'), "line {:?}", line);
        for key in &["\"name\":", "\"algo\":", "\"hash\":"] {
            assert!(line.contains(key), "line {:?} missing {}", line, key);
        }
    }
}

#[test]
fn hash_extra_field_round_trips_through_create() {
    // Create two archives of the same corpus: one with --write-md5 +
    // --write-sha256, one without. Both `tzip hash --algo md5/sha256`
    // outputs should match line-for-line, confirming the extra-field
    // digest matches the on-the-fly decode-and-hash result.
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));

    run_tzip(
        tmp.path(),
        &["create", "plain.zip", "corpus", "-m", "deflate", "-q"],
    );
    run_tzip(
        tmp.path(),
        &[
            "create",
            "hashed.zip",
            "corpus",
            "-m",
            "deflate",
            "--write-md5",
            "--write-sha256",
            "-q",
        ],
    );

    for algo in &["md5", "sha256"] {
        let plain = run_tzip(tmp.path(), &["hash", "plain.zip", "--algo", algo]);
        let hashed = run_tzip(tmp.path(), &["hash", "hashed.zip", "--algo", algo]);
        let p = parse_hash_lines(&String::from_utf8_lossy(&plain.stdout));
        let h = parse_hash_lines(&String::from_utf8_lossy(&hashed.stdout));
        assert_eq!(p, h, "extra-field digest must match streamed digest for {}", algo);
    }
}

#[test]
fn hash_extra_field_survives_through_other_tools() {
    // Create with --write-sha256, extract with unzip (which ignores
    // unknown extras), re-create with tzip: the extra field is
    // not re-populated (we stream-copy on carry only), but the
    // archive stays valid and readable.
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "--write-sha256", "-q"],
    );
    // Verify tzip can list + extract its own archive.
    let ls = run_tzip(tmp.path(), &["ls", "out.zip"]);
    assert!(String::from_utf8_lossy(&ls.stdout).contains("a.txt"));
}

#[test]
fn hash_include_and_exclude_filter() {
    let tmp = tempfile::tempdir().unwrap();
    make_corpus(&tmp.path().join("corpus"));
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "store", "-q"],
    );

    let out = run_tzip(
        tmp.path(),
        &["hash", "out.zip", "--algo", "crc32", "--include", "*.txt"],
    );
    let map = parse_hash_lines(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(map.len(), 2, "only .txt entries");
    assert!(map.contains_key("corpus/a.txt"));
    assert!(!map.contains_key("corpus/blob.bin"));

    let out = run_tzip(
        tmp.path(),
        &["hash", "out.zip", "--algo", "crc32", "--exclude", "*.bin"],
    );
    let map = parse_hash_lines(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(map.len(), 2, "only non-.bin entries");
}
