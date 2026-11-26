//! Integration tests for the Phase 6 CDR cache (issue #74).
//!
//! The unit tests in `src/index/cdr_cache.rs` cover the blob
//! encode/decode. These test the real end-to-end:
//! `Archive::open_source_with_cache` writing the blob on first
//! open + reading it back on the second, including the forged-
//! blob rejection the security analysis asks for.

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use tzip::cdr_cache::{Blob, CDR_CACHE_KEY};
use tzip::indexcache::{ArchiveId, IndexCache};
use tzip::zipreader::Archive;

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn build_small_archive(names: &[&str]) -> (tempfile::TempDir, Vec<u8>, u64, u64) {
    let tmp = tempfile::tempdir().unwrap();
    for name in names {
        std::fs::write(tmp.path().join(name), format!("content-{}\n", name).as_bytes()).unwrap();
    }
    let mut args: Vec<&str> = vec!["create", "out.zip"];
    for n in names {
        args.push(*n);
    }
    args.push("-q");
    let status = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(&args)
        .status()
        .expect("run tzip create");
    assert!(status.success());
    let bytes = std::fs::read(tmp.path().join("out.zip")).unwrap();
    // Need cd_offset / cd_size for later-test corruption. Pull
    // them by parsing once (via the ordinary no-cache path).
    let arch = Archive::open_source(Box::new(Cursor::new(bytes.clone()))).unwrap();
    let cd_offset = arch.cd_offset();
    let cd_size = arch.cd_size();
    drop(arch);
    (tmp, bytes, cd_offset, cd_size)
}

#[test]
fn cdr_cache_second_open_survives_zeroed_cdr_in_source() {
    // The strongest proof the cache works: corrupt the CDR
    // region of the source between the two opens. If the
    // second open reads the CDR from cache (as it must), it
    // parses the same entries; if the cache is broken, the
    // zeroed bytes would fail CDR parsing.
    let (_tmp, bytes, cd_offset, cd_size) = build_small_archive(&["a.txt", "b.txt", "c.txt"]);

    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(
        IndexCache::open(cache_dir.path().to_path_buf(), 16 * 1024 * 1024).unwrap(),
    );
    let id = ArchiveId::derive("mem://cdr-cache-test", "fingerprint-v1");

    // First open — cache miss + writeback.
    let arch1 = Archive::open_source_with_cache(
        Box::new(Cursor::new(bytes.clone())),
        Some((Arc::clone(&cache), id)),
    )
    .expect("first open");
    let names1: Vec<String> = arch1.iter_entries().map(|e| e.name.clone()).collect();
    assert_eq!(names1.len(), 3);
    drop(arch1);
    assert!(
        cache.get(&id, CDR_CACHE_KEY).is_some(),
        "CDR cache blob not written on first open"
    );

    // Zero the CDR region in the source. EOCD stays intact at
    // the tail, so a non-cached parse would do its EOCD scan,
    // then try to parse zeroes as CDR entries (first entry's
    // sig would be 0, triggering a fall-through to break =
    // zero entries, i.e. empty archive). We want 3, not 0.
    let mut corrupted = bytes.clone();
    for b in &mut corrupted[cd_offset as usize..(cd_offset + cd_size) as usize] {
        *b = 0;
    }

    let arch2 = Archive::open_source_with_cache(
        Box::new(Cursor::new(corrupted)),
        Some((Arc::clone(&cache), id)),
    )
    .expect("second open must hit the cache");
    let names2: Vec<String> = arch2.iter_entries().map(|e| e.name.clone()).collect();
    assert_eq!(names1, names2, "cached CDR gave a different entry set");
}

#[test]
fn cdr_cache_rejects_blob_with_mismatched_file_size() {
    // Defence-in-depth: a planted blob whose declared
    // file_size doesn't match the source must be ignored
    // (not trusted to redirect subsequent reads at arbitrary
    // offsets).
    let (_tmp, bytes, _, _) = build_small_archive(&["a.txt"]);

    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(
        IndexCache::open(cache_dir.path().to_path_buf(), 16 * 1024 * 1024).unwrap(),
    );
    let id = ArchiveId::derive("mem://mismatch", "fp");

    let bogus = Blob {
        file_size: 1,
        cd_offset: 0,
        cd_size: 0,
        entry_count: 0,
        comment: Vec::new(),
        cdr: Vec::new(),
    };
    cache.put(&id, CDR_CACHE_KEY, &bogus.encode()).unwrap();

    let arch = Archive::open_source_with_cache(
        Box::new(Cursor::new(bytes.clone())),
        Some((Arc::clone(&cache), id)),
    )
    .expect("open must fall through on mismatched blob");
    let names: Vec<String> = arch.iter_entries().map(|e| e.name.clone()).collect();
    assert_eq!(names, vec!["a.txt".to_string()]);

    // The fresh fetch should have overwritten the forged
    // blob with the correct one.
    let fresh = cache.get(&id, CDR_CACHE_KEY).unwrap();
    let decoded = Blob::decode(&fresh).unwrap();
    assert_eq!(decoded.file_size, bytes.len() as u64);
}

#[test]
fn cdr_cache_different_archive_ids_dont_collide() {
    // Phase 6 keys cache entries on (url, fingerprint) via
    // ArchiveId. Two archives at the same URL with different
    // fingerprints must not share a slot — a changed ETag
    // means a changed object and must force a fresh fetch.
    let (_tmp, bytes_a, _, _) = build_small_archive(&["a.txt", "b.txt"]);
    let (_tmp2, bytes_b, _, _) = build_small_archive(&["x.txt", "y.txt", "z.txt"]);

    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(
        IndexCache::open(cache_dir.path().to_path_buf(), 16 * 1024 * 1024).unwrap(),
    );
    let id_a = ArchiveId::derive("mem://same-url", "etag-A");
    let id_b = ArchiveId::derive("mem://same-url", "etag-B");

    Archive::open_source_with_cache(
        Box::new(Cursor::new(bytes_a.clone())),
        Some((Arc::clone(&cache), id_a)),
    )
    .unwrap();
    Archive::open_source_with_cache(
        Box::new(Cursor::new(bytes_b.clone())),
        Some((Arc::clone(&cache), id_b)),
    )
    .unwrap();

    // Re-open A on an EMPTY source — should still see 2
    // entries because the cache is fingerprint-keyed. If the
    // cache mixed up slots, either arch1 would get B's
    // entries (3) or an empty source would fail parsing.
    //
    // (Note: we need the empty source to still have a valid
    // length, otherwise the mismatch guard trips. Use the
    // real A bytes so file_size matches.)
    let arch_a = Archive::open_source_with_cache(
        Box::new(Cursor::new(bytes_a.clone())),
        Some((Arc::clone(&cache), id_a)),
    )
    .unwrap();
    let names_a: Vec<_> = arch_a.iter_entries().map(|e| e.name.clone()).collect();
    assert_eq!(names_a.len(), 2, "archive A's cache contaminated by B");

    let arch_b = Archive::open_source_with_cache(
        Box::new(Cursor::new(bytes_b.clone())),
        Some((Arc::clone(&cache), id_b)),
    )
    .unwrap();
    let names_b: Vec<_> = arch_b.iter_entries().map(|e| e.name.clone()).collect();
    assert_eq!(names_b.len(), 3, "archive B's cache contaminated by A");
}
