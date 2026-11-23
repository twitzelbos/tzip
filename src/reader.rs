//! Reader subcommands: `tzip ls`, `tzip cat`, `tzip x`.
//!
//! These form Increment 1 of the reader stack: everything operates on a
//! local file. Future increments (HTTP Range, S3, mount) swap in different
//! `Read + Seek` sources but reuse the same per-entry pipeline (locate
//! body range from CDR → stream decrypt → stream decompress).

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File};
use std::io::{BufWriter, Read, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use crate::cli::{Method, ReaderOp, ReaderOptions};
use crate::crypto::{self, AE2_OVERHEAD};
use crate::decompress;
use crate::sources::{http::HttpSource, s3::S3Source};
use crate::zipreader::{Archive, ReadEntry};

/// Dispatches on scheme: `s3://…` → `S3Source`, `http(s)://…` → `HttpSource`,
/// anything else → local file path. Called by every reader subcommand so
/// local and remote behave identically downstream of this seam.
fn open_archive(spec: &str) -> Result<Archive> {
    if let Some(rest) = spec.strip_prefix("s3://") {
        let src = S3Source::open(rest)
            .with_context(|| format!("open S3 source s3://{}", rest))?;
        Archive::open_source(Box::new(src))
    } else if spec.starts_with("http://") || spec.starts_with("https://") {
        let src = HttpSource::open(spec)
            .with_context(|| format!("open HTTP source {}", spec))?;
        Archive::open_source(Box::new(src))
    } else {
        Archive::open(Path::new(spec))
    }
}

// ── Entry points ───────────────────────────────────────────────────────────

pub fn run_ls(opts: ReaderOptions) -> Result<()> {
    let arch = open_archive(&opts.archive)
        .with_context(|| format!("open {}", opts.archive))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    writeln!(
        out,
        "  Length    Compressed  Method    Modified             Name"
    )?;
    writeln!(
        out,
        "----------  ----------  -------   ----------------  --  ----"
    )?;

    let mut total_uncomp = 0u64;
    let mut total_comp = 0u64;
    for e in arch.entries() {
        let method = method_label(e);
        let encrypted_mark = if e.encrypted { "*" } else { " " };
        let dt = format_dos_mtime(e.mtime);
        writeln!(
            out,
            "{:>10}  {:>10}  {:<9} {}  {} {}",
            e.uncomp_size, e.comp_size, method, dt, encrypted_mark, e.name,
        )?;
        total_uncomp += e.uncomp_size;
        total_comp += e.comp_size;
    }
    writeln!(
        out,
        "----------  ----------                                   -------"
    )?;
    writeln!(
        out,
        "{:>10}  {:>10}                                    {} files",
        total_uncomp,
        total_comp,
        arch.entries().len()
    )?;
    Ok(())
}

pub fn run_cat(opts: ReaderOptions) -> Result<()> {
    let name = match &opts.op {
        ReaderOp::Cat { name } => name.clone(),
        _ => unreachable!("run_cat called with wrong op"),
    };
    let mut arch = open_archive(&opts.archive)
        .with_context(|| format!("open {}", opts.archive))?;

    let idx = find_entry_index(&arch, &name)?;
    arch.lfh_total_size_of(idx)?;
    let re = arch.entries()[idx].clone();

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    stream_entry(&mut arch, &re, opts.password.as_deref(), &mut out)?;
    Ok(())
}

pub fn run_extract(opts: ReaderOptions) -> Result<()> {
    let (to, names_filter) = match &opts.op {
        ReaderOp::Extract { to, names } => (to.clone(), names.clone()),
        _ => unreachable!("run_extract called with wrong op"),
    };

    fs::create_dir_all(&to)
        .with_context(|| format!("create extraction root {}", to.display()))?;
    let to = fs::canonicalize(&to)
        .with_context(|| format!("canonicalize {}", to.display()))?;

    let mut arch = open_archive(&opts.archive)
        .with_context(|| format!("open {}", opts.archive))?;

    let selected_set: Option<std::collections::HashSet<String>> = if names_filter.is_empty() {
        None
    } else {
        Some(names_filter.iter().cloned().collect())
    };

    let n = arch.entries().len();
    let mut extracted = 0usize;
    for idx in 0..n {
        {
            let re_ref = &arch.entries()[idx];
            if let Some(sel) = &selected_set {
                if !sel.contains(&re_ref.name) {
                    continue;
                }
            }
        }
        // Populate lfh_total_size before cloning so the clone carries it.
        arch.lfh_total_size_of(idx)?;
        let re = arch.entries()[idx].clone();
        extract_one(&mut arch, &re, &to, opts.password.as_deref(), opts.verbose)?;
        extracted += 1;
    }

    if let Some(sel) = &selected_set {
        if extracted != sel.len() {
            bail!(
                "extracted {} of {} requested entries (some names did not match)",
                extracted,
                sel.len()
            );
        }
    }
    if !opts.quiet {
        eprintln!("extracted {} entries to {}", extracted, to.display());
    }
    Ok(())
}

// ── Shared per-entry pipeline ──────────────────────────────────────────────

fn extract_one(
    arch: &mut Archive,
    re: &ReadEntry,
    root: &Path,
    password: Option<&str>,
    verbose: bool,
) -> Result<()> {
    let dest = safe_join(root, &re.name)
        .with_context(|| format!("entry name {:?}", re.name))?;

    // If the entry represents a directory (trailing / and zero body), create
    // the directory and move on.
    if re.name.ends_with('/') && re.uncomp_size == 0 {
        fs::create_dir_all(&dest)
            .with_context(|| format!("mkdir {}", dest.display()))?;
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }

    // Stream into a BufWriter. On any error, delete the partial output so
    // the caller sees an atomic failure.
    let write_result = (|| -> Result<u64> {
        let f = File::create(&dest)
            .with_context(|| format!("create {}", dest.display()))?;
        let mut buf = BufWriter::new(f);
        let n = stream_entry(arch, re, password, &mut buf)?;
        buf.flush().context("flush extract buffer")?;
        Ok(n)
    })();

    match write_result {
        Ok(_) => {
            if verbose {
                eprintln!("extracted {}", re.name);
            }
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&dest);
            Err(e).with_context(|| format!("extract {}", re.name))
        }
    }
}

fn stream_entry<W: Write>(
    arch: &mut Archive,
    re: &ReadEntry,
    password: Option<&str>,
    dst: &mut W,
) -> Result<u64> {
    let body_offset = re.lfh_offset + re.lfh_total_size as u64;
    arch.source_mut()
        .seek(SeekFrom::Start(body_offset))
        .with_context(|| format!("seek to body of {}", re.name))?;

    let inner_method = method_from_id(re.inner_method)
        .ok_or_else(|| anyhow!("unsupported compression method {} on {}", re.inner_method, re.name))?;

    if re.encrypted {
        let password = password.ok_or_else(|| {
            anyhow!(
                "entry {} is AES-encrypted — pass --password / -p (or - to prompt)",
                re.name
            )
        })?;
        let ciphertext_len = re
            .comp_size
            .checked_sub(AE2_OVERHEAD as u64)
            .ok_or_else(|| anyhow!("entry {} too small to be AE-2", re.name))?;

        // Decrypt into a buffered pipe, then decompress out of that pipe.
        // We pass the plaintext through an in-memory Vec for simplicity;
        // for multi-GB entries this is still bounded by the 64 KiB working
        // buffer inside `decrypt_ae2` + a single whole-plaintext allocation.
        // A future optimization: wire crypto::decrypt_ae2 as a Read adapter
        // instead of materializing the plaintext.
        let mut plaintext: Vec<u8> = Vec::with_capacity(re.uncomp_size.min(64 * 1024 * 1024) as usize);
        crypto::decrypt_ae2(password, arch.source_mut(), ciphertext_len, &mut plaintext)?;

        let written = decompress::decompress(
            inner_method,
            std::io::Cursor::new(&plaintext),
            re.uncomp_size,
            dst,
        )?;
        Ok(written)
    } else {
        // Non-encrypted: feed the archive body directly through decompress,
        // bounded to `re.comp_size` so a corrupt entry can't drag in bytes
        // from the next entry. `(&mut *source).take(n)` wraps the source
        // without needing to clone it — works for Box<dyn ReadSeek> where
        // `try_clone` isn't available.
        let bounded = arch.source_mut().take(re.comp_size);
        let written = decompress::decompress(inner_method, bounded, re.uncomp_size, dst)?;
        Ok(written)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

fn find_entry_index(arch: &Archive, name: &str) -> Result<usize> {
    arch.entries()
        .iter()
        .position(|e| e.name == name)
        .ok_or_else(|| anyhow!("no entry named {:?} in archive", name))
}

fn method_label(e: &ReadEntry) -> &'static str {
    match method_from_id(e.inner_method) {
        Some(Method::Store) => "store",
        Some(Method::Deflate) => "deflate",
        Some(Method::Bzip2) => "bzip2",
        Some(Method::Lzma) => "lzma",
        Some(Method::Xz) => "xz",
        Some(Method::Zstd) => "zstd",
        None => "?",
    }
}

fn method_from_id(id: u16) -> Option<Method> {
    Some(match id {
        0 => Method::Store,
        8 => Method::Deflate,
        12 => Method::Bzip2,
        14 => Method::Lzma,
        93 => Method::Zstd,
        95 => Method::Xz,
        _ => return None,
    })
}

fn format_dos_mtime((date, time): (u16, u16)) -> String {
    // DOS date = (year-1980) << 9 | month << 5 | day
    // DOS time = hour << 11 | minute << 5 | sec/2
    let year = 1980 + ((date >> 9) & 0x7F) as u32;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as u32;
    let minute = ((time >> 5) & 0x3F) as u32;
    let sec = ((time & 0x1F) << 1) as u32;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hour, minute, sec
    )
}

/// Resolve an entry's archive path to a filesystem path under `root`,
/// rejecting zip-slip patterns (`..`, absolute paths, Windows drive
/// letters, UNC paths). Normalized via lexical join only — no filesystem
/// lookup that could follow a symlink out of `root`.
fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    // Reject absolute paths, drive letters, UNC prefixes outright.
    if name.starts_with('/') || name.starts_with('\\') {
        bail!("entry name is absolute: {:?}", name);
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        bail!("entry name looks like a Windows drive path: {:?}", name);
    }

    let mut out = root.to_path_buf();
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            bail!("entry name contains `..`: {:?}", name);
        }
        // Also reject components that would resolve to anything parent-
        // ish when the join happens.
        let p = Path::new(seg);
        for c in p.components() {
            match c {
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("entry segment is unsafe: {:?}", seg);
                }
                Component::CurDir => {}
                Component::Normal(_) => {}
            }
        }
        out.push(seg);
    }
    Ok(out)
}
