//! Update-mode end-to-end tests: --update (append + rebuild), --delete,
//! --atomic, AES preservation, and undo-file recovery.
//!
//! Follows the same pattern as `tests/roundtrip.rs`: tzip is launched as a
//! subprocess with the tempdir as its cwd so relative paths (the `zip -r`
//! convention) resolve correctly.

use std::fs;
use std::io::Read;
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
        "tzip failed: args={:?} stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn run_tzip_expect_failure(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run tzip");
    assert!(
        !out.status.success(),
        "expected tzip to fail but it succeeded: args={:?}",
        args
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Returns stderr. Useful for asserting mode choice ("append" vs "rebuild").
fn run_tzip_capture(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn entry_names(archive: &Path) -> Vec<String> {
    let f = fs::File::open(archive).unwrap();
    let mut zf = zip::ZipArchive::new(f).unwrap();
    (0..zf.len())
        .map(|i| zf.by_index_raw(i).unwrap().name().to_string())
        .collect()
}

fn entry_contents(archive: &Path, name: &str) -> Vec<u8> {
    let f = fs::File::open(archive).unwrap();
    let mut zf = zip::ZipArchive::new(f).unwrap();
    let mut e = zf.by_name(name).unwrap();
    let mut buf = Vec::new();
    e.read_to_end(&mut buf).unwrap();
    buf
}

// ── add / replace / delete, two-mode dispatch ──────────────────────────────

#[test]
fn update_add_append_mode() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"hello a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), b"hello b\n").unwrap();

    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "-q"]);
    let stderr = run_tzip_capture(tmp.path(), &["u", "out.zip", "b.txt"]);
    // b.txt is already in the archive — would force rebuild. Use a new file.
    assert!(
        !stderr.contains("update mode = append"),
        "re-adding existing entry should force rebuild; stderr={}",
        stderr
    );

    fs::write(tmp.path().join("c.txt"), b"hello c\n").unwrap();
    let stderr = run_tzip_capture(tmp.path(), &["u", "out.zip", "c.txt"]);
    assert!(
        stderr.contains("update mode = append"),
        "pure add should use append mode; stderr={}",
        stderr
    );

    let names = entry_names(&tmp.path().join("out.zip"));
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
    assert_eq!(entry_contents(&tmp.path().join("out.zip"), "c.txt"), b"hello c\n");

    // Undo file must be gone on success.
    assert!(
        !tmp.path().join("out.zip.tzip-undo").exists(),
        "undo file should be removed after successful update"
    );
}

#[test]
fn update_add_rebuild_mode_explicit_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"hello a\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);

    fs::write(tmp.path().join("b.txt"), b"hello b\n").unwrap();
    let stderr = run_tzip_capture(tmp.path(), &["u", "--atomic", "out.zip", "b.txt"]);
    assert!(
        stderr.contains("update mode = rebuild"),
        "--atomic should force rebuild; stderr={}",
        stderr
    );

    // No temp files left behind.
    for entry in fs::read_dir(tmp.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.contains("tzip-tmp"),
            "temp file left behind: {}",
            name
        );
    }

    let names = entry_names(&tmp.path().join("out.zip"));
    assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string()]);
}

#[test]
fn update_replace_forces_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"version 1\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);

    fs::write(tmp.path().join("a.txt"), b"version 2 is longer\n").unwrap();
    let stderr = run_tzip_capture(tmp.path(), &["u", "out.zip", "a.txt"]);
    assert!(stderr.contains("update mode = rebuild"));

    assert_eq!(
        entry_contents(&tmp.path().join("out.zip"), "a.txt"),
        b"version 2 is longer\n"
    );
}

#[test]
fn update_delete_forces_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    fs::write(tmp.path().join("c.txt"), b"c\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "c.txt", "-q"]);

    let stderr = run_tzip_capture(tmp.path(), &["u", "--delete", "b.txt", "out.zip"]);
    assert!(stderr.contains("update mode = rebuild"));

    let names = entry_names(&tmp.path().join("out.zip"));
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
    assert!(!names.contains(&"b.txt".to_string()));
}

#[test]
fn update_mixed_delete_and_add() {
    let tmp = tempfile::tempdir().unwrap();
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(tmp.path().join(n), format!("{}\n", n).as_bytes()).unwrap();
    }
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "c.txt", "-q"]);

    fs::write(tmp.path().join("d.txt"), b"d\n").unwrap();
    run_tzip(
        tmp.path(),
        &["u", "--delete", "b.txt", "out.zip", "d.txt"],
    );

    let mut names = entry_names(&tmp.path().join("out.zip"));
    names.sort();
    assert_eq!(
        names,
        vec!["a.txt".to_string(), "c.txt".to_string(), "d.txt".to_string()]
    );
}

#[test]
fn update_delete_only_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    for n in ["a.txt", "b.txt"] {
        fs::write(tmp.path().join(n), format!("{}\n", n).as_bytes()).unwrap();
    }
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "-q"]);

    run_tzip(tmp.path(), &["u", "--delete", "b.txt", "out.zip"]);

    let names = entry_names(&tmp.path().join("out.zip"));
    assert_eq!(names, vec!["a.txt".to_string()]);
}

// ── AES preservation ───────────────────────────────────────────────────────

#[test]
fn update_preserves_aes_on_kept_entries() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"secret a\n").unwrap();
    run_tzip(
        tmp.path(),
        &["create", "out.zip", "a.txt", "-p", "hunter2", "-q"],
    );

    // Snapshot the AES body of 'a' BEFORE the update.
    let before = fs::read(tmp.path().join("out.zip")).unwrap();

    fs::write(tmp.path().join("b.txt"), b"secret b\n").unwrap();
    run_tzip(
        tmp.path(),
        &["u", "out.zip", "b.txt", "-p", "hunter2"],
    );

    // Append mode truncates at `before`'s cd_offset and appends new LFHs.
    // That means `after[..before_cd_offset]` must be byte-identical to
    // `before[..before_cd_offset]` — every LFH + body including `a`'s AES
    // envelope (salt, pbkdf2-derived verify, ciphertext, HMAC) is untouched.
    let after = fs::read(tmp.path().join("out.zip")).unwrap();
    let (cd_offset_before, _) = find_eocd_cd_offset(&before);
    assert_eq!(
        &before[..cd_offset_before as usize],
        &after[..cd_offset_before as usize],
        "AES-encrypted entry 'a' was mutated by append-mode update"
    );

    // Verify via `zip` crate that both entries exist.
    let names = entry_names(&tmp.path().join("out.zip"));
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
}

#[test]
fn update_mixes_plain_and_aes() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("plain.txt"), b"plain content\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "plain.txt", "-q"]);

    fs::write(tmp.path().join("secret.txt"), b"secret content\n").unwrap();
    run_tzip(
        tmp.path(),
        &["u", "out.zip", "secret.txt", "-p", "pw"],
    );

    // Verify via 7z (`zip` crate can enumerate both). Just check names.
    let names = entry_names(&tmp.path().join("out.zip"));
    assert!(names.contains(&"plain.txt".to_string()));
    assert!(names.contains(&"secret.txt".to_string()));
}

// ── misc ───────────────────────────────────────────────────────────────────

#[test]
fn update_sort_produces_sorted_cdr() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "b.txt", "a.txt", "--sort", "-q"]);

    fs::write(tmp.path().join("c.txt"), b"c\n").unwrap();
    run_tzip(tmp.path(), &["u", "--sort", "out.zip", "c.txt"]);

    let names = entry_names(&tmp.path().join("out.zip"));
    assert_eq!(
        names,
        vec!["a.txt".to_string(), "b.txt".to_string(), "c.txt".to_string()]
    );
}

#[test]
fn update_creates_if_archive_missing() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip(tmp.path(), &["u", "fresh.zip", "a.txt", "-q"]);
    assert!(tmp.path().join("fresh.zip").exists());
    let names = entry_names(&tmp.path().join("fresh.zip"));
    assert_eq!(names, vec!["a.txt".to_string()]);
}

#[test]
fn update_rejects_7z() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    let stderr = run_tzip_expect_failure(tmp.path(), &["u", "out.7z", "a.txt"]);
    assert!(
        stderr.to_lowercase().contains(".7z"),
        "expected a .7z error, got: {}",
        stderr
    );
}

#[test]
fn update_rejects_empty_op() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);
    let stderr = run_tzip_expect_failure(tmp.path(), &["u", "out.zip"]);
    assert!(
        stderr.to_lowercase().contains("delete") || stderr.to_lowercase().contains("input"),
        "expected a validation error, got: {}",
        stderr
    );
}

// ── undo-file recovery ─────────────────────────────────────────────────────

#[test]
fn update_removes_stale_undo_file() {
    // Fabricate an undo file that matches the archive's current state (i.e.
    // the prior run completed but failed to remove the undo file). The next
    // -u invocation should silently delete it before doing its work.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "-q"]);

    let archive = tmp.path().join("out.zip");
    let undo = tmp.path().join("out.zip.tzip-undo");
    write_fake_undo_matching_archive(&archive, &undo);

    fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    run_tzip(tmp.path(), &["u", "out.zip", "b.txt"]);

    assert!(!undo.exists(), "stale undo should have been removed");
    let names = entry_names(&archive);
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
}

#[test]
fn update_recovers_from_crashed_append() {
    // Fabricate a crashed-append state: the archive has been truncated to
    // cd_offset but the new CDR was never written. The undo file holds the
    // original CDR+EOCD. Next tzip -u must restore the tail and succeed.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    run_tzip(tmp.path(), &["create", "out.zip", "a.txt", "b.txt", "-q"]);

    let archive = tmp.path().join("out.zip");
    let undo = tmp.path().join("out.zip.tzip-undo");

    // 1. Read the original archive and find cd_offset via its EOCD.
    let bytes = fs::read(&archive).unwrap();
    let (cd_offset, _entry_count) = find_eocd_cd_offset(&bytes);

    // 2. Write undo = [magic | version=1 | orig_size | cd_offset | tail].
    let tail = &bytes[cd_offset as usize..];
    let mut undo_bytes = Vec::new();
    undo_bytes.extend_from_slice(b"TZIPUNDO");
    undo_bytes.extend_from_slice(&1u32.to_le_bytes());
    undo_bytes.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    undo_bytes.extend_from_slice(&cd_offset.to_le_bytes());
    undo_bytes.extend_from_slice(tail);
    fs::write(&undo, &undo_bytes).unwrap();

    // 3. Truncate the archive to cd_offset (simulate crash mid-append).
    let truncated = &bytes[..cd_offset as usize];
    fs::write(&archive, truncated).unwrap();

    // 4. Run tzip -u. It should restore, then append our new entry.
    fs::write(tmp.path().join("c.txt"), b"c\n").unwrap();
    let stderr = run_tzip_capture(tmp.path(), &["u", "out.zip", "c.txt"]);
    assert!(
        stderr.to_lowercase().contains("recovered"),
        "expected recovery message, got: {}",
        stderr
    );

    assert!(!undo.exists());
    let names = entry_names(&archive);
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
}

// ── helpers ────────────────────────────────────────────────────────────────

fn write_fake_undo_matching_archive(archive: &Path, undo: &Path) {
    let bytes = fs::read(archive).unwrap();
    let (cd_offset, _n) = find_eocd_cd_offset(&bytes);
    let tail = &bytes[cd_offset as usize..];
    let mut u = Vec::new();
    u.extend_from_slice(b"TZIPUNDO");
    u.extend_from_slice(&1u32.to_le_bytes());
    u.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    u.extend_from_slice(&cd_offset.to_le_bytes());
    u.extend_from_slice(tail);
    fs::write(undo, &u).unwrap();
}

#[test]
fn rebuild_drops_stale_indices_and_regenerates_names() {
    // ChatGPT review #6: before the fix, replace/delete rebuilds
    // byte-copied the stale `.tzip/*` sentinels from the source.
    // Those sentinels indexed entries by CDR position, so the
    // rebuilt archive had an index pointing at the wrong rows
    // (and, for `.tzip/names.v1`, a lookup table whose string
    // values decoded into the middle of a different entry's LFH).
    //
    // After the fix every `.tzip/*` is stripped at rebuild time;
    // `.tzip/names.v1` is regenerated automatically over the final
    // kept + new entry list. Term / facet / columnar sidecars are
    // dropped with a stderr note (not re-emitted; would require
    // re-tokenising kept bodies).
    let tmp = tempfile::tempdir().unwrap();
    for n in ["a.json", "b.json", "c.json"] {
        fs::write(
            tmp.path().join(n),
            format!("{{\"name\":\"{}\",\"val\":1}}\n", n).as_bytes(),
        )
        .unwrap();
    }
    run_tzip(
        tmp.path(),
        &[
            "create",
            "out.zip",
            "a.json",
            "b.json",
            "c.json",
            "--index-names",
            "--index-terms",
            "*.json",
            "-q",
        ],
    );
    let before = entry_names(&tmp.path().join("out.zip"));
    assert!(before.iter().any(|n| n == ".tzip/names.v1"));
    assert!(before.iter().any(|n| n == ".tzip/terms.v1"));

    // --delete forces rebuild mode. The rebuilt archive must not
    // carry the old terms sentinel (its positions now reference
    // the wrong rows) and must carry a *fresh* names sentinel.
    run_tzip(tmp.path(), &["u", "--delete", "b.json", "out.zip"]);

    let after = entry_names(&tmp.path().join("out.zip"));
    assert!(
        !after.iter().any(|n| n == ".tzip/terms.v1"),
        "rebuild left a stale .tzip/terms.v1 behind: {:?}",
        after
    );
    assert!(
        after.iter().any(|n| n == ".tzip/names.v1"),
        "rebuild failed to regenerate .tzip/names.v1: {:?}",
        after
    );
    assert!(after.iter().any(|n| n == "a.json"));
    assert!(after.iter().any(|n| n == "c.json"));
    assert!(!after.iter().any(|n| n == "b.json"));

    // And the regenerated name index actually resolves to the
    // final entries (not an off-by-one into a different LFH):
    // tzip's own `ls` uses the index to find entries.
    let ls = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(["ls", "out.zip"])
        .output()
        .expect("run tzip ls");
    assert!(ls.status.success(), "tzip ls failed after rebuild");
    let listing = String::from_utf8_lossy(&ls.stdout);
    assert!(listing.contains("a.json"));
    assert!(listing.contains("c.json"));
    assert!(!listing.contains("b.json"));
}

/// Minimal EOCD scan: returns (cd_offset, entry_count) from the last 64 KiB
/// of the archive. Does not handle ZIP64 — fine for the small test archives
/// we produce.
fn find_eocd_cd_offset(bytes: &[u8]) -> (u64, u16) {
    let sig = 0x06054b50u32.to_le_bytes();
    let window = bytes.len().min(64 * 1024 + 22);
    let start = bytes.len() - window;
    let buf = &bytes[start..];
    let max_i = buf.len() - 22;
    for i in (0..=max_i).rev() {
        if buf[i..i + 4] == sig {
            let comment_len = u16::from_le_bytes([buf[i + 20], buf[i + 21]]) as usize;
            if i + 22 + comment_len == buf.len() {
                let entry_count = u16::from_le_bytes([buf[i + 10], buf[i + 11]]);
                let cd_offset =
                    u32::from_le_bytes([buf[i + 16], buf[i + 17], buf[i + 18], buf[i + 19]]);
                return (cd_offset as u64, entry_count);
            }
        }
    }
    panic!("EOCD not found in test archive");
}

/// Issue #94: `tzip u` used to call `lfh_total_size_of` on
/// every kept entry *before* deciding append vs rebuild,
/// reading 30 bytes per kept entry regardless of which path
/// it ended up on. For a 48 K-entry archive on a remote
/// backend, that's 48 K latency-bound reads for no reason.
///
/// Regression: build an archive, zero out the first 30 bytes
/// at every kept entry's lfh_offset (corrupting the LFH
/// signature so any probe would fail), then run `tzip u` in
/// pure-append mode. If the append path still touches those
/// bytes, `probe LFH for carried entry …` would blow up.
#[test]
fn append_mode_does_not_probe_kept_entry_lfhs() {
    let tmp = tempfile::tempdir().unwrap();
    // 20 small kept entries so there are many LFHs to
    // potentially probe. Store method so zeroing the LFH
    // sig is unambiguous (no decompressor reads these
    // bytes during append).
    for i in 0..20 {
        fs::write(tmp.path().join(format!("k{:02}.txt", i)), b"body\n").unwrap();
    }
    let paths: Vec<String> = (0..20).map(|i| format!("k{:02}.txt", i)).collect();
    let mut args: Vec<&str> = vec!["create", "out.zip", "-m", "store", "-q"];
    for p in &paths {
        args.push(p.as_str());
    }
    run_tzip(tmp.path(), &args);

    // Recover each kept entry's lfh_offset by parsing the
    // archive once via tzip's own reader.
    let archive_path = tmp.path().join("out.zip");
    let offsets: Vec<u64> = {
        use tzip::zipreader::Archive;
        let arch = Archive::open(&archive_path).unwrap();
        arch.iter_entries().map(|e| e.lfh_offset).collect()
    };
    assert_eq!(offsets.len(), 20);

    // Zero out the first 30 bytes at each offset (the LFH
    // fixed header). The archive is now corrupt from the
    // POV of anything that probes LFHs — but a pure-append
    // `tzip u` shouldn't care, because kept bytes are
    // only copied in bulk, not parsed, in append mode.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&archive_path)
            .unwrap();
        for off in &offsets {
            f.seek(SeekFrom::Start(*off)).unwrap();
            f.write_all(&[0u8; 30]).unwrap();
        }
    }

    // Add one new entry via append mode. If the pre-#94 code
    // path ran, it would hit `probe LFH for carried entry
    // k00.txt` and bail with "bad LFH signature at offset
    // …: 0x0". The fix makes it skip that loop entirely.
    fs::write(tmp.path().join("new.txt"), b"appended\n").unwrap();
    let out = Command::new(tzip_bin())
        .current_dir(tmp.path())
        .args(["u", "out.zip", "new.txt"])
        .output()
        .expect("run tzip u");
    assert!(
        out.status.success(),
        "tzip u aborted — the pre-#94 code is still probing LFHs:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Confirm the new entry landed. Can't use the `zip` crate
    // (its `by_index_raw` validates LFH signatures on open —
    // those are deliberately zeroed), so go through tzip's own
    // reader which parses CDR only.
    use tzip::zipreader::Archive;
    let arch = Archive::open(&archive_path).unwrap();
    let names: Vec<String> = arch.iter_entries().map(|e| e.name.clone()).collect();
    assert!(
        names.iter().any(|n| n == "new.txt"),
        "new.txt not found after append: {:?}",
        names
    );
}
