//! P0 #4 — end-to-end archive integrity & crash testing.
//!
//! The existing `update_recovers_from_crashed_append` test covers
//! one truncation point (cd_offset exactly). This file sweeps the
//! full append region: truncate at N offsets between `cd_offset`
//! and `file_size`, each simulating a different moment the write
//! could have been interrupted. After `tzip u` runs recovery,
//! every entry's CRC-32 must match the pre-append archive and the
//! whole archive must remain parseable. A corrupted / short undo
//! file must be rejected (never silently serve the wrong bytes),
//! and a stray rebuild-temp file left behind by a previous run
//! must not confuse the next one.

use std::collections::BTreeMap;
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

fn run_tzip_capturing(cwd: &Path, args: &[&str]) -> (bool, String, String) {
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

/// Minimal EOCD scan (non-ZIP64), identical to the helper in
/// `tests/zip/update.rs`.
fn find_eocd_cd_offset(bytes: &[u8]) -> u64 {
    let sig = 0x06054b50u32.to_le_bytes();
    let window = bytes.len().min(64 * 1024 + 22);
    let start = bytes.len() - window;
    let buf = &bytes[start..];
    let max_i = buf.len() - 22;
    for i in (0..=max_i).rev() {
        if buf[i..i + 4] == sig {
            let comment_len = u16::from_le_bytes([buf[i + 20], buf[i + 21]]) as usize;
            if i + 22 + comment_len == buf.len() {
                let cd_offset =
                    u32::from_le_bytes([buf[i + 16], buf[i + 17], buf[i + 18], buf[i + 19]]);
                return cd_offset as u64;
            }
        }
    }
    panic!("EOCD not found in test archive");
}

fn crc32_map(cwd: &Path, archive: &str) -> BTreeMap<String, String> {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(["hash", "--algo", "crc32", archive])
        .output()
        .expect("run tzip hash");
    assert!(
        out.status.success(),
        "hash failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let mut m = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut sp = line.splitn(2, "  ");
        let hex = sp.next().unwrap().to_string();
        let name = sp.next().unwrap().to_string();
        m.insert(name, hex);
    }
    m
}

/// Build an archive with enough variety to exercise LFH-boundary
/// edge cases: entries bigger than a few KiB, entries at
/// non-aligned boundaries, mix of names that the writer sorts
/// before CDR emission.
fn make_base_archive(tmp: &Path) {
    fs::create_dir_all(tmp.join("corpus")).unwrap();
    for (i, name) in ["alpha.txt", "beta.txt", "gamma.txt", "delta.txt"]
        .iter()
        .enumerate()
    {
        let body: Vec<u8> = (0..(1500 + i * 97) as u16)
            .map(|n| (n.wrapping_mul(31).wrapping_add(7)) as u8)
            .collect();
        fs::write(tmp.join("corpus").join(name), body).unwrap();
    }
    run_tzip(tmp, &["create", "out.zip", "corpus", "-m", "deflate", "-q"]);
}

/// Make a crashed-append snapshot: truncate the archive to
/// `cd_offset + extra` and write a correct undo file capturing
/// the original tail. Mirrors `local.rs::write_undo_file` + the
/// pre-truncate step that `run_update_append` does on the hot
/// path.
fn stage_crashed_append(archive: &Path, undo: &Path, truncate_at: u64) {
    let bytes = fs::read(archive).unwrap();
    let cd_offset = find_eocd_cd_offset(&bytes);
    assert!(
        truncate_at >= cd_offset,
        "truncate_at must be past cd_offset (append region)",
    );
    let tail = &bytes[cd_offset as usize..];
    let mut u = Vec::new();
    u.extend_from_slice(b"TZIPUNDO");
    u.extend_from_slice(&1u32.to_le_bytes());
    u.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    u.extend_from_slice(&cd_offset.to_le_bytes());
    u.extend_from_slice(tail);
    fs::write(undo, &u).unwrap();
    let truncated = &bytes[..truncate_at as usize];
    fs::write(archive, truncated).unwrap();
}

#[test]
fn crash_matrix_every_offset_recovers_and_preserves_entries() {
    // Sweep truncation offsets between cd_offset and file_size
    // (the "mid-append" window). For each one `tzip u` must
    // recognise the undo file, restore the tail, add the new
    // entry, and leave every original entry's CRC-32 intact.
    let tmp = tempfile::tempdir().unwrap();
    make_base_archive(tmp.path());
    let archive = tmp.path().join("out.zip");
    let undo = tmp.path().join("out.zip.tzip-undo");

    let before = crc32_map(tmp.path(), "out.zip");
    let orig_bytes = fs::read(&archive).unwrap();
    let cd_offset = find_eocd_cd_offset(&orig_bytes);
    let total = orig_bytes.len() as u64;

    // Enumerate the append-region: cd_offset, +64B, +256B, …
    // every step up to the end of the tail.
    let sweep: Vec<u64> = {
        let region = total - cd_offset;
        let mut v = vec![cd_offset];
        for step in [16u64, 64, 128, 256, 512, 1024, 2048] {
            if step < region {
                v.push(cd_offset + step);
            }
        }
        // And one offset near the end — simulates "crash during
        // EOCD write" rather than "crash during CDR write."
        if region > 4 {
            v.push(cd_offset + region - 4);
        }
        v
    };

    for (i, &truncate_at) in sweep.iter().enumerate() {
        // Reset the archive to its pre-crash state.
        fs::write(&archive, &orig_bytes).unwrap();
        stage_crashed_append(&archive, &undo, truncate_at);

        fs::write(
            tmp.path().join(format!("extra_{i}.txt")),
            format!("iter-{i} payload\n").as_bytes(),
        )
        .unwrap();
        let (ok, _stdout, stderr) = run_tzip_capturing(
            tmp.path(),
            &["u", "out.zip", &format!("extra_{i}.txt")],
        );
        assert!(
            ok,
            "iter {i} (truncate_at={truncate_at}) failed: {stderr}",
        );
        assert!(
            stderr.to_lowercase().contains("recovered"),
            "iter {i} did not emit recovery message: {stderr}",
        );
        assert!(
            !undo.exists(),
            "iter {i}: undo file should be gone after successful recovery",
        );

        // Every original entry's CRC-32 must survive recovery.
        let after = crc32_map(tmp.path(), "out.zip");
        for (name, hex) in &before {
            assert_eq!(
                after.get(name),
                Some(hex),
                "iter {i} truncate_at={truncate_at}: entry {name} CRC changed through recovery",
            );
        }
        // And the new entry we just added must be present.
        assert!(
            after.contains_key(&format!("extra_{i}.txt")),
            "iter {i}: new entry missing after recovery: {after:?}",
        );
    }
}

#[test]
fn crash_undo_rejects_short_header() {
    // A corrupted / truncated undo file (shorter than the fixed
    // header prefix) must be rejected cleanly. Silently falling
    // through to "no recovery needed" and then letting an
    // append clobber the archive would be the worst-of-all-
    // worlds outcome.
    let tmp = tempfile::tempdir().unwrap();
    make_base_archive(tmp.path());
    let archive = tmp.path().join("out.zip");
    let undo = tmp.path().join("out.zip.tzip-undo");

    // Fabricate a one-byte undo file.
    fs::write(&undo, &[0xFFu8]).unwrap();

    fs::write(tmp.path().join("extra.txt"), b"extra\n").unwrap();
    let (ok, _stdout, stderr) = run_tzip_capturing(
        tmp.path(),
        &["u", "out.zip", "extra.txt"],
    );
    assert!(
        !ok,
        "a corrupted undo file must abort, not silently proceed; stderr: {stderr}",
    );
    // And the archive is untouched — we must not have let a
    // partial append mutate the original.
    let names: Vec<String> = crc32_map(tmp.path(), "out.zip")
        .keys()
        .cloned()
        .collect();
    assert!(
        names.iter().any(|n| n == "corpus/alpha.txt"),
        "archive mutated under corrupted undo: {names:?}",
    );
    assert!(
        !names.iter().any(|n| n == "extra.txt"),
        "append should not have succeeded with corrupted undo: {names:?}",
    );
}

#[test]
fn crash_undo_rejects_wrong_magic() {
    // Right size, wrong magic bytes. Same contract: refuse to
    // interpret an unknown undo file as an instruction to rewrite
    // the archive.
    let tmp = tempfile::tempdir().unwrap();
    make_base_archive(tmp.path());
    let archive = tmp.path().join("out.zip");
    let undo = tmp.path().join("out.zip.tzip-undo");

    // Magic "NOPENOPE" instead of "TZIPUNDO".
    let bytes = fs::read(&archive).unwrap();
    let cd_offset = find_eocd_cd_offset(&bytes);
    let tail = &bytes[cd_offset as usize..];
    let mut u = Vec::new();
    u.extend_from_slice(b"NOPENOPE");
    u.extend_from_slice(&1u32.to_le_bytes());
    u.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    u.extend_from_slice(&cd_offset.to_le_bytes());
    u.extend_from_slice(tail);
    fs::write(&undo, &u).unwrap();

    fs::write(tmp.path().join("extra.txt"), b"extra\n").unwrap();
    let (ok, _stdout, stderr) = run_tzip_capturing(
        tmp.path(),
        &["u", "out.zip", "extra.txt"],
    );
    assert!(
        !ok,
        "unknown undo magic must abort; stderr: {stderr}",
    );
}

#[test]
fn stale_rebuild_tempfile_does_not_block_next_run() {
    // Rebuild mode writes to `<archive>.tzip-tmp-<pid>` and
    // `rename()`s on success. A stray tempfile left behind by a
    // previous crash must be ignored (not clobber our live run,
    // not get picked up as "the real archive").
    let tmp = tempfile::tempdir().unwrap();
    make_base_archive(tmp.path());

    // Fabricate a stale rebuild-tempfile. The real pattern is
    // `<name>.tzip-tmp-<pid>`; any pid value does.
    let stale = tmp.path().join("out.zip.tzip-tmp-999999");
    fs::write(&stale, b"garbage that pretends to be an archive").unwrap();

    // Force the rebuild path (replace an existing entry).
    fs::write(
        tmp.path().join("corpus").join("alpha.txt"),
        b"replacement content\n",
    )
    .unwrap();
    run_tzip(tmp.path(), &["u", "out.zip", "corpus/alpha.txt"]);

    // The archive is still at out.zip and still parseable.
    let after = crc32_map(tmp.path(), "out.zip");
    assert!(
        after.contains_key("corpus/alpha.txt"),
        "after rebuild, replacement entry missing: {after:?}",
    );
    // (The stale temp doesn't need to be cleaned up by this
    // run — it was from a previous process. The key invariant
    // is that it didn't interfere with ours.)
    assert!(tmp.path().join("out.zip").exists());
}
