//! `tzip hash` — compute a plaintext hash for every entry in an archive.
//!
//! Two speeds:
//!
//! - `--algo crc32` reads the entry's CRC-32 straight from the
//!   central directory. Zero body I/O; free for HTTP / S3 archives.
//!
//! - `--algo md5` / `--algo sha256` streams each entry's body through
//!   a decompressor + hasher in parallel workers, mirroring the model
//!   `tzip x` uses for extract. For AES-encrypted entries the body is
//!   decrypted first, same as `tzip cat`.

use anyhow::{anyhow, Context, Result};
use md5::Md5;
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::cli::{HashAlgo, HashArgs};
use crate::reader::{
    hint_from_archive, open_archive, open_source_for_with_hint, stream_entry_to,
};
use crate::zipreader::ReadEntry;

pub fn run(args: HashArgs) -> Result<()> {
    let password = resolve_password(&args)?;

    let mut arch = open_archive(&args.archive)
        .with_context(|| format!("open {}", args.archive))?;

    // Build the include/exclude filter + explicit-names filter.
    let selected: HashSet<String> = if args.names.is_empty() {
        HashSet::new()
    } else {
        args.names.iter().cloned().collect()
    };
    if !selected.is_empty() {
        let present: HashSet<&str> =
            arch.entries().iter().map(|e| e.name.as_str()).collect();
        let missing: Vec<&String> = selected
            .iter()
            .filter(|n| !present.contains(n.as_str()))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "entries not in archive: {}",
                missing
                    .iter()
                    .map(|s| format!("{:?}", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // Collect the entry indices we'll hash, dropping directory entries
    // (empty bodies) because a directory has no meaningful hash.
    let candidates: Vec<usize> = (0..arch.entries().len())
        .filter(|&i| {
            let e = &arch.entries()[i];
            if e.name.ends_with('/') && e.uncomp_size == 0 {
                return false;
            }
            if !selected.is_empty() && !selected.contains(&e.name) {
                return false;
            }
            passes_globs(&e.name, &args.include, &args.exclude)
        })
        .collect();

    if candidates.is_empty() {
        return Ok(());
    }

    // Fast path: CRC32 lives in the CDR, no body work needed.
    if matches!(args.algo, HashAlgo::Crc32) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for idx in &candidates {
            let e = &arch.entries()[*idx];
            emit_line(&mut out, args.algo, &e.name, &format!("{:08x}", e.crc32), args.json)?;
        }
        return Ok(());
    }

    // Body-streaming path. For entries that carry a pre-computed
    // digest in the tzip-native extra field (written via
    // `tzip create --write-md5` / `--write-sha256`) we can skip the
    // decompress + hash round-trip entirely and emit straight from
    // the CDR — same cost as the crc32 fast path. Entries without
    // a stored digest fall through to the worker pool.
    let stdout = io::stdout();
    let mut direct_out = stdout.lock();
    let mut entries: Vec<ReadEntry> = Vec::with_capacity(candidates.len());
    for idx in candidates {
        let (name, stored) = {
            let e = &arch.entries()[idx];
            let stored = match args.algo {
                HashAlgo::Md5 => e.md5_digest.map(|d| hex_of(&d)),
                HashAlgo::Sha256 => e.sha256_digest.map(|d| hex_of(&d)),
                HashAlgo::Crc32 => unreachable!("crc32 handled above"),
            };
            (e.name.clone(), stored)
        };
        if let Some(hex) = stored {
            emit_line(&mut direct_out, args.algo, &name, &hex, args.json)?;
            continue;
        }
        arch.lfh_total_size_of(idx)?;
        entries.push(arch.entries()[idx].clone());
    }
    drop(direct_out);
    if entries.is_empty() {
        return Ok(());
    }
    let worker_hint = hint_from_archive(&args.archive, &arch);
    drop(arch);

    let parallel = args.parallel.max(1).min(entries.len());
    let (tx, rx) = crossbeam_channel::unbounded::<ReadEntry>();
    for re in entries {
        let _ = tx.send(re);
    }
    drop(tx);

    // Owning Stdout (not StdoutLock) in the Mutex — `StdoutLock` isn't
    // Send. Each worker calls `.lock()` for the per-write handle.
    let out: Mutex<io::Stdout> = Mutex::new(io::stdout());
    let algo = args.algo;
    let json = args.json;
    let verbose = args.verbose;
    let password = password;
    let spec = args.archive.clone();
    let hashed = AtomicUsize::new(0);

    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::with_capacity(parallel);
        for _ in 0..parallel {
            let rx = rx.clone();
            let spec = spec.clone();
            let password = password.clone();
            let out = &out;
            let hashed = &hashed;
            let handle = scope.spawn(move || -> Result<()> {
                let mut source = open_source_for_with_hint(&spec, worker_hint)
                    .with_context(|| format!("open worker source for {}", spec))?;
                while let Ok(re) = rx.recv() {
                    let hex = hash_one(
                        &mut *source,
                        &re,
                        password.as_deref(),
                        algo,
                    )
                    .with_context(|| format!("hash {}", re.name))?;
                    let guard = out.lock().expect("stdout mutex");
                    let mut handle = guard.lock();
                    emit_line(&mut handle, algo, &re.name, &hex, json)?;
                    drop(handle);
                    drop(guard);
                    hashed.fetch_add(1, Ordering::Relaxed);
                    if verbose {
                        eprintln!("hashed {}", re.name);
                    }
                }
                Ok(())
            });
            handles.push(handle);
        }
        let mut first_err: Option<anyhow::Error> = None;
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(anyhow!("hash worker thread panicked"));
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })?;

    if !args.quiet && args.verbose {
        eprintln!("hashed {} entries", hashed.into_inner());
    }
    Ok(())
}

fn hash_one(
    source: &mut dyn crate::sources::ReadSeek,
    re: &ReadEntry,
    password: Option<&str>,
    algo: HashAlgo,
) -> Result<String> {
    let mut sink = HashSink::new(algo);
    stream_entry_to(source, re, password, &mut sink)?;
    Ok(sink.finalize_hex())
}

fn emit_line<W: Write>(
    w: &mut W,
    algo: HashAlgo,
    name: &str,
    hex: &str,
    json: bool,
) -> io::Result<()> {
    if json {
        writeln!(
            w,
            "{{\"name\":{},\"algo\":\"{}\",\"hash\":\"{}\"}}",
            json_str(name),
            algo.label(),
            hex
        )
    } else {
        writeln!(w, "{}  {}", hex, name)
    }
}

/// Minimal JSON string escaper. Enough for ZIP entry names (Unicode,
/// backslashes, quotes). We don't need the full JSON spec because
/// the format is one object per line with known keys.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn resolve_password(args: &HashArgs) -> Result<Option<String>> {
    if let Some(ref pw) = args.password {
        if pw == "-" {
            let pw = rpassword::prompt_password("password: ")
                .context("read password from prompt")?;
            return Ok(Some(pw));
        }
        return Ok(Some(pw.clone()));
    }
    if let Some(ref f) = args.password_file {
        let s = std::fs::read_to_string(f)
            .with_context(|| format!("read password-file {}", f.display()))?;
        return Ok(Some(s.trim_end_matches(&['\n', '\r'][..]).to_string()));
    }
    Ok(None)
}

/// A `Write` adapter that feeds bytes into one of the supported
/// hashers. `stream_entry_to` wants `&mut dyn Write`, so we give it
/// one of these per entry; `finalize_hex()` consumes it to produce
/// the lowercase-hex digest.
enum HashSink {
    Md5(Md5),
    Sha256(Sha256),
}

impl HashSink {
    fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Md5 => HashSink::Md5(Md5::new()),
            HashAlgo::Sha256 => HashSink::Sha256(Sha256::new()),
            HashAlgo::Crc32 => unreachable!("crc32 is handled on the fast path"),
        }
    }
    fn finalize_hex(self) -> String {
        match self {
            HashSink::Md5(h) => hex_of(&h.finalize()),
            HashSink::Sha256(h) => hex_of(&h.finalize()),
        }
    }
}

impl Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            HashSink::Md5(h) => h.update(buf),
            HashSink::Sha256(h) => h.update(buf),
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Shell-glob include/exclude match on an entry name. `*` matches any
/// sequence (including `/`); `?` matches a single char. Mirrors
/// `tzip find`'s behaviour without exposing its internal filter type.
fn passes_globs(name: &str, include: &[String], exclude: &[String]) -> bool {
    if !include.is_empty() && !include.iter().any(|g| glob_match(g, name)) {
        return false;
    }
    !exclude.iter().any(|g| glob_match(g, name))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn recur(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if recur(&p[1..], t) {
                    return true;
                }
                if t.is_empty() {
                    return false;
                }
                recur(p, &t[1..])
            }
            (Some(b'?'), Some(_)) => recur(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => recur(&p[1..], &t[1..]),
            _ => false,
        }
    }
    recur(pattern.as_bytes(), text.as_bytes())
}

fn hex_of(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}
