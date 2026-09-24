//! `.7z` solid output via `sevenz-rust`.
//!
//! Trades tzip's per-file parallelism for a single LZMA2 solid stream that
//! yields substantially better ratios on similar-file corpora (DICOM, logs,
//! source trees). Reads still go through our reader pool → we throttle disk
//! contention even when the compressor is serial.
//!
//! ## Known upstream limitation
//!
//! `sevenz-rust 0.6` does not expose multi-threaded LZMA2 (`-mmt=on` in
//! p7zip parlance). That mode splits the solid stream into independent
//! LZMA2 blocks that can be compressed in parallel, at a ~2-5% ratio cost.
//! Until sevenz-rust ships that API (tracked issue upstream), `.7z --solid`
//! is single-threaded regardless of `-j`.
//!
//! Workarounds if speed matters more than ratio on `.7z`:
//! - use `-m xz` inside a `.zip` (per-file LZMA, still parallel across files)
//! - shell out to system `p7zip` with `7z a -mmt=on -m0=lzma2 ...`

use anyhow::{Context, Result};
use sevenz_rust::{
    lzma::LZMA2Options, AesEncoderOptions, Password, SevenZArchiveEntry, SevenZMethod,
    SevenZMethodConfiguration, SevenZWriter,
};
use std::fs::File;
use std::io::Cursor;

use crate::cli::Options;
use crate::platform;
use crate::progress::Progress;
use crate::walker::WorkItem;

pub fn write_solid(
    opts: &Options,
    items: &[WorkItem],
    total_bytes: u64,
    total_files: u64,
) -> Result<()> {
    let out = File::create(&opts.archive)
        .with_context(|| format!("create archive {}", opts.archive.display()))?;
    let mut writer = SevenZWriter::new(out).context("open 7z writer")?;

    // Content methods:
    // - encrypted: AES-256/SHA-256 then LZMA2 (matches p7zip's `-mhe=on`)
    // - plain: LZMA2 only
    let mut lzma_opts = LZMA2Options::with_preset(opts.level.clamp(0, 9));
    // Bigger dict = better ratio; the user's `--dict-size` slot would land here.
    let _ = &mut lzma_opts;
    let methods: Vec<SevenZMethodConfiguration> = match opts.password.as_deref() {
        Some(pw) if !pw.is_empty() => vec![
            AesEncoderOptions::new(Password::from(pw)).into(),
            SevenZMethodConfiguration::new(SevenZMethod::LZMA2).with_options(
                sevenz_rust::MethodOptions::from(lzma_opts),
            ),
        ],
        _ => vec![SevenZMethodConfiguration::new(SevenZMethod::LZMA2).with_options(
            sevenz_rust::MethodOptions::from(lzma_opts),
        )],
    };
    writer.set_content_methods(methods);
    writer.set_encrypt_header(opts.password.is_some());

    if opts.verbose && opts.cpu_jobs > 1 {
        eprintln!(
            "note: .7z solid is single-threaded (sevenz-rust 0.6 lacks MT-LZMA2); --jobs {} unused",
            opts.cpu_jobs
        );
    }

    let progress = Progress::new(total_bytes, total_files, opts.quiet && !opts.tui);

    let mut written = 0u64;
    for item in items {
        let bytes = platform::read_input(&item.path, item.size, opts.keep_cache)
            .with_context(|| format!("read {}", item.path.display()))?;
        let plain_len = bytes.len() as u64;
        let entry = SevenZArchiveEntry::from_path(&item.path, item.name_in_archive.clone());
        // sevenz-rust wants an owned Read; we clone into a Vec here for large
        // files (the mmap path). No cheap way around this without patching
        // sevenz-rust to accept &[u8]. See feedback #8 in the perf notes.
        let vec_bytes: Vec<u8> = bytes.to_vec();
        writer
            .push_archive_entry(entry, Some(Cursor::new(vec_bytes)))
            .with_context(|| format!("compress {}", item.name_in_archive))?;
        written += 1;
        progress.inc(plain_len);
        progress.set_msg(format!(
            "{}/{} files — {}",
            written, total_files, item.name_in_archive
        ));
        if opts.verbose {
            eprintln!("added {}", item.name_in_archive);
        }
    }

    writer.finish().context("finalize 7z archive")?;
    progress.finish("done");
    Ok(())
}
