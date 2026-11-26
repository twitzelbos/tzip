//! `.tzip/manifest.v1` round-trip: `tzip create` emits a manifest
//! that enumerates every index sentinel the archive carries;
//! `Archive::manifest()` parses it back.

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

fn make_json_corpus(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for i in 0..6 {
        let status = [200, 404, 500, 200, 500, 200][i];
        let user = ["alice", "bob", "carol"][i % 3];
        let content = format!(
            "{{\"id\":{i},\"status_code\":{status},\"user\":\"{user}\",\"msg\":\"ok operation happened\"}}\n"
        );
        fs::write(root.join(format!("e{:03}.json", i)), content).unwrap();
    }
}

#[test]
fn create_emits_manifest_listing_every_index_component() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    make_json_corpus(&corpus);

    run_tzip(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "corpus",
            "-m",
            "deflate",
            "--index-names",
            "--index-terms",
            "*.json",
            "--facet",
            ".status_code",
            "--facet",
            ".user",
            "--write-columnar",
            "-q",
        ],
    );

    // Read manifest through the public `Archive` API.
    let arch_path = tmp.path().join("out.zip");
    let mut arch = tzip::zipreader::Archive::open(&arch_path).expect("open archive");
    let manifest = arch.manifest().expect("manifest should be present");
    assert_eq!(manifest.schema(), tzip::manifest::SCHEMA_VERSION);
    assert!(manifest.has_names(), "names index should be listed");
    assert!(manifest.has_terms(), "terms index should be listed");
    assert!(
        manifest.has_columnar(),
        "columnar sidecar should be listed"
    );
    let facet_fields: std::collections::BTreeSet<&str> = manifest.facet_fields().collect();
    assert!(
        facet_fields.contains("status_code"),
        "status_code facet missing: {:?}",
        facet_fields
    );
    assert!(
        facet_fields.contains("user"),
        "user facet missing: {:?}",
        facet_fields
    );
}

#[test]
fn plain_archive_has_no_manifest() {
    // No --index-* flags → archive carries no sentinels → no
    // manifest. `.manifest()` returns None cleanly.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    fs::write(corpus.join("a.txt"), b"hello\n").unwrap();

    run_tzip(
        tmp.path(),
        &["create", "out.zip", "corpus", "-m", "deflate", "-q"],
    );

    let arch_path = tmp.path().join("out.zip");
    let mut arch = tzip::zipreader::Archive::open(&arch_path).expect("open archive");
    assert!(
        arch.manifest().is_none(),
        "plain archive must not emit a manifest",
    );
}

#[test]
fn manifest_decodes_by_tzip_cat() {
    // The manifest is just another ZIP entry — users can `tzip
    // cat` it and get the text form we committed to in
    // src/index/manifest.rs's doc comment. Guards against silent
    // format drift.
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    make_json_corpus(&corpus);
    run_tzip(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "corpus",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "--facet",
            ".status_code",
            "-q",
        ],
    );
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(["cat", "out.zip", ".tzip/manifest.v1"])
        .output()
        .expect("run tzip cat");
    assert!(out.status.success(), "cat manifest failed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("schema = 1"), "schema line missing: {text}");
    assert!(text.contains("terms = 1"), "terms line missing: {text}");
    assert!(
        text.contains("facets.status_code = 1"),
        "facet line missing: {text}"
    );
    // Columnar / names were NOT requested, so those lines
    // should not appear.
    assert!(
        !text.contains("columnar ="),
        "columnar unexpectedly present: {text}"
    );
    assert!(
        !text.contains("names ="),
        "names unexpectedly present: {text}"
    );
}
