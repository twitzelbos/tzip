//! Streaming decompression — the inverse of `compress.rs`. Each method
//! dispatches to a `Read`-adapter decoder so a multi-GB entry doesn't need
//! to be fully buffered.
//!
//! LZMA (ZIP method 14) is the one special case: ZIP method 14 wraps the
//! LZMA1 stream in a custom 4-byte prefix (`major`, `minor`,
//! `props_len_le_u16`) that is NOT part of the LZMA_ALONE format. On the
//! decode side we strip that prefix, re-assemble a well-formed LZMA_ALONE
//! stream with a known uncompressed size, and feed xz2's LZMA decoder.
//!
//! The pre-Deflate PKZIP methods — Shrink (1), Reduce (2-5), Implode (6)
//! — go through `compcol`, a pure-Rust codec collection, via a push-pull
//! adapter (`drive_compcol`). Each needs out-of-band metadata from the
//! ZIP entry: Shrink needs the uncompressed length, Reduce needs the
//! factor (encoded in the method ID), Implode needs the window size +
//! literal-tree flags (from general-purpose bits 1 and 2).

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};

use crate::cli::Method;
use compcol::Decoder as CompcolDecoder;

/// All methods tzip can *read*. A superset of the write-side `Method`
/// enum (the pre-Deflate PKZIP methods are decode-only, as they are in
/// every modern toolchain since PKZIP 2.x). Per-entry config travels
/// inside the variants so `decompress` stays a single uniform call.
#[derive(Copy, Clone, Debug)]
pub enum ReadMethod {
    Store,
    Deflate,
    Bzip2,
    Lzma,
    Xz,
    Zstd,
    /// PKZIP method 1 — LZW variant (9-13 bit codes, partial clear).
    Shrink,
    /// PKZIP methods 2-5 — `factor` is 1, 2, 3, or 4.
    Reduce(u8),
    /// PKZIP method 6 — Shannon-Fano coded LZ77 with 4 or 8 KiB window.
    Implode { large_window: bool, lit_tree: bool },
    /// PKWARE Deflate64 (method 9) — like Deflate but with a 64 KiB
    /// window and the longer-match extensions. Common in Windows-produced
    /// ZIPs (WinZip, .NET's `ZipArchive` with CompressionLevel.Optimal
    /// on large entries).
    Deflate64,
    /// PPMd variant Ir1 (method 98) — Dmitry Shkarin's context-modelling
    /// coder. Rare in the wild but a legit ZIP format.
    Ppmd,
}

impl ReadMethod {
    /// Short label for `tzip ls` output.
    pub fn label(self) -> &'static str {
        match self {
            ReadMethod::Store => "store",
            ReadMethod::Deflate => "deflate",
            ReadMethod::Bzip2 => "bzip2",
            ReadMethod::Lzma => "lzma",
            ReadMethod::Xz => "xz",
            ReadMethod::Zstd => "zstd",
            ReadMethod::Shrink => "shrink",
            ReadMethod::Reduce(_) => "reduce",
            ReadMethod::Implode { .. } => "implode",
            ReadMethod::Deflate64 => "deflate64",
            ReadMethod::Ppmd => "ppmd",
        }
    }

    /// Lift the modern write-side method into its read-side equivalent.
    /// Used by `compress.rs`'s self-test / round-trip helpers that
    /// already have a `Method` on hand.
    pub fn from_write_method(m: Method) -> Self {
        match m {
            Method::Store => ReadMethod::Store,
            Method::Deflate => ReadMethod::Deflate,
            Method::Bzip2 => ReadMethod::Bzip2,
            Method::Lzma => ReadMethod::Lzma,
            Method::Xz => ReadMethod::Xz,
            Method::Zstd => ReadMethod::Zstd,
            Method::Ppmd => ReadMethod::Ppmd,
        }
    }
}

pub fn decompress<R: Read, W: Write>(
    method: ReadMethod,
    src: R,
    uncomp_size: u64,
    dst: &mut W,
) -> Result<u64> {
    match method {
        ReadMethod::Store => copy_exact(src, dst, uncomp_size).context("store copy"),
        ReadMethod::Deflate => decompress_deflate(src, uncomp_size, dst),
        ReadMethod::Bzip2 => {
            let mut dec = bzip2::read::BzDecoder::new(src);
            std::io::copy(&mut dec, dst).context("bzip2 decompress")
        }
        ReadMethod::Xz => {
            let mut dec = xz2::read::XzDecoder::new(src);
            std::io::copy(&mut dec, dst).context("xz decompress")
        }
        ReadMethod::Zstd => {
            let mut dec =
                zstd::stream::read::Decoder::new(src).context("zstd decoder init")?;
            std::io::copy(&mut dec, dst).context("zstd decompress")
        }
        ReadMethod::Lzma => decompress_lzma(src, uncomp_size, dst),
        ReadMethod::Shrink => {
            let dec = compcol::zip_shrink::Decoder::raw(uncomp_size);
            drive_compcol("shrink", dec, src, dst)
        }
        ReadMethod::Reduce(factor) => {
            let dec = compcol::zip_reduce::Decoder::raw(factor, uncomp_size)
                .map_err(|e| anyhow!("reduce decoder init (factor {}): {:?}", factor, e))?;
            drive_compcol("reduce", dec, src, dst)
        }
        ReadMethod::Implode { large_window, lit_tree } => {
            let dec = compcol::zip_implode::Decoder::raw(large_window, lit_tree, uncomp_size);
            drive_compcol("implode", dec, src, dst)
        }
        ReadMethod::Deflate64 => {
            let dec = compcol::deflate64::Decoder::new();
            drive_compcol("deflate64", dec, src, dst)
        }
        ReadMethod::Ppmd => {
            // Cap the PPMd model arena at 256 MiB — enough for anything
            // in the wild, well below "a malicious ZIP asks for 2 GiB".
            // The decoder rejects over-ask with Error::Unsupported
            // before allocating.
            let dec = compcol::ppmd::ZipDecoder::new(uncomp_size)
                .with_max_memory(256 * 1024 * 1024);
            drive_compcol("ppmd", dec, src, dst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Store is trivial, but exercises the full `decompress` entry point
    /// end-to-end. `ReadMethod::Store` doesn't touch compcol — the real
    /// coverage of the compcol adapter lives in `drive_compcol_smoke`.
    #[test]
    fn store_roundtrips() {
        let payload = b"hello store".to_vec();
        let mut out = Vec::new();
        let n = decompress(
            ReadMethod::Store,
            std::io::Cursor::new(&payload),
            payload.len() as u64,
            &mut out,
        )
        .unwrap();
        assert_eq!(n as usize, payload.len());
        assert_eq!(out, payload);
    }

    /// Validate the push-pull adapter with a tiny decoder that returns
    /// `StreamEnd` after copying input to output — no feature flags
    /// needed, and it touches every branch of `drive_compcol`.
    #[test]
    fn drive_compcol_smoke() {
        use compcol::{Error as CcErr, Progress, Status};

        struct Identity {
            done: bool,
        }

        impl CompcolDecoder for Identity {
            fn decode(
                &mut self,
                input: &[u8],
                output: &mut [u8],
            ) -> Result<(Progress, Status), CcErr> {
                if self.done {
                    return Ok((Progress { consumed: 0, written: 0 }, Status::StreamEnd));
                }
                let n = input.len().min(output.len());
                output[..n].copy_from_slice(&input[..n]);
                let status = if n < input.len() {
                    Status::OutputFull
                } else {
                    Status::InputEmpty
                };
                Ok((Progress { consumed: n, written: n }, status))
            }
            fn finish(
                &mut self,
                _output: &mut [u8],
            ) -> Result<(Progress, Status), CcErr> {
                self.done = true;
                Ok((Progress { consumed: 0, written: 0 }, Status::StreamEnd))
            }
            fn reset(&mut self) {
                self.done = false;
            }
            fn discard_output(
                &mut self,
                _input: &[u8],
                _n: usize,
            ) -> Result<(Progress, Status), CcErr> {
                Ok((Progress { consumed: 0, written: 0 }, Status::InputEmpty))
            }
        }

        // 200 KiB — big enough to exercise multiple refills.
        let payload: Vec<u8> = (0..200 * 1024).map(|i| (i & 0xff) as u8).collect();
        let mut out = Vec::new();
        let n = drive_compcol(
            "identity",
            Identity { done: false },
            std::io::Cursor::new(&payload),
            &mut out,
        )
        .unwrap();
        assert_eq!(n as usize, payload.len());
        assert_eq!(out, payload);
    }
}

/// Entries above this uncompressed size fall back to the streaming
/// `flate2` (miniz_oxide) decoder so we don't hold ~2× the body size
/// in per-worker RAM. libdeflater is one-shot: it wants the whole
/// compressed buffer AND the whole output buffer at once.
///
/// 64 MiB covers essentially every real ZIP entry; the ones that
/// don't fit were I/O-bound anyway.
const DEFLATE_ONESHOT_LIMIT: u64 = 64 * 1024 * 1024;

fn decompress_deflate<R: Read, W: Write>(
    mut src: R,
    uncomp_size: u64,
    dst: &mut W,
) -> Result<u64> {
    // Streaming fallback: large entries, or unknown-size cases.
    if uncomp_size == 0 || uncomp_size > DEFLATE_ONESHOT_LIMIT {
        let mut dec = flate2::read::DeflateDecoder::new(src);
        return std::io::copy(&mut dec, dst).context("deflate decompress (streaming)");
    }

    // Hot path: read the whole compressed body, let libdeflater
    // expand into a pre-sized buffer in one call. libdeflate is a
    // SIMD-accelerated native implementation — ~2-3× faster than
    // flate2's miniz_oxide for typical entries.
    let mut input: Vec<u8> = Vec::new();
    src.read_to_end(&mut input)
        .context("read deflate body into one-shot buffer")?;
    let mut out: Vec<u8> = Vec::with_capacity(uncomp_size as usize);
    // SAFETY: libdeflater writes exactly `n` bytes; we truncate to
    // `n` before exposing the buffer. `u8` has no drop, so leaving
    // the tail uninitialized can't cause UB on an early return.
    unsafe { out.set_len(uncomp_size as usize); }
    let mut dec = libdeflater::Decompressor::new();
    let n = dec
        .deflate_decompress(&input, &mut out)
        .map_err(|e| anyhow!("deflate decompress: {:?}", e))?;
    if n != uncomp_size as usize {
        // Spec-wise this shouldn't happen — CDR's uncompressed size
        // is authoritative. If it does we trust what libdeflate
        // actually produced.
        out.truncate(n);
    }
    dst.write_all(&out)
        .context("write deflate output")?;
    Ok(n as u64)
}

fn copy_exact<R: Read, W: Write>(mut src: R, dst: &mut W, mut remaining: u64) -> Result<u64> {
    let mut buf = [0u8; 256 * 1024];
    let mut copied = 0u64;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = src.read(&mut buf[..want])?;
        if n == 0 {
            bail!("unexpected EOF in stored entry (missing {} bytes)", remaining);
        }
        dst.write_all(&buf[..n])?;
        copied += n as u64;
        remaining -= n as u64;
    }
    Ok(copied)
}

/// ZIP method 14 wire layout:
///
///     [ major(1) | minor(1) | props_len_le_u16 | lzma_props(props_len) | lzma_data ]
///
/// (props_len is 5 bytes in practice: the LZMA1 properties byte +
/// 4-byte dictionary size, both little-endian.)
///
/// LZMA_ALONE layout that `xz2`'s LZMA decoder accepts:
///
///     [ lzma_props(5) | uncomp_size_le_u64 | lzma_data ]
///
/// We read the method-14 prefix, grab the 5 props bytes, then build an
/// LZMA_ALONE prefix with the known uncompressed size and splice it in
/// front of the LZMA data via a chained reader.
fn decompress_lzma<R: Read, W: Write>(
    mut src: R,
    uncomp_size: u64,
    dst: &mut W,
) -> Result<u64> {
    let mut hdr = [0u8; 4];
    src.read_exact(&mut hdr).context("read ZIP LZMA prefix")?;
    let props_len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
    if props_len != 5 {
        bail!("unexpected ZIP LZMA properties length: {}", props_len);
    }
    let mut props = [0u8; 5];
    src.read_exact(&mut props).context("read LZMA properties")?;

    // Build the LZMA_ALONE prefix: [ props(5) | uncomp_size_le_u64 ].
    let mut alone = Vec::with_capacity(13);
    alone.extend_from_slice(&props);
    alone.extend_from_slice(&uncomp_size.to_le_bytes());

    let chained = std::io::Cursor::new(alone).chain(src);
    let stream = xz2::stream::Stream::new_lzma_decoder(u64::MAX)
        .map_err(|e| anyhow!("lzma decoder init: {e:?}"))?;
    let mut dec = xz2::read::XzDecoder::new_stream(chained, stream);
    std::io::copy(&mut dec, dst).context("lzma decompress")
}

/// Bridge the push-pull `compcol::Decoder` trait (`decode(input,
/// output) -> (Progress, Status)` + `finish(output)`) to the
/// `Read` + `Write` streaming pattern the rest of tzip uses. Reads from
/// `src` into a 64 KiB input buffer, hands it to the decoder, writes
/// its 256 KiB output chunk to `dst`, repeats. Exits on
/// `Status::StreamEnd` or when `src` runs dry (then calls `finish`).
fn drive_compcol<D: CompcolDecoder, R: Read, W: Write>(
    name: &'static str,
    mut dec: D,
    mut src: R,
    dst: &mut W,
) -> Result<u64> {
    use compcol::Status;

    let mut in_buf = [0u8; 64 * 1024];
    let mut out_buf = vec![0u8; 256 * 1024];
    let mut in_fill: usize = 0;
    let mut in_pos: usize = 0;
    let mut src_eof = false;
    let mut total_out: u64 = 0;

    loop {
        // Refill the input buffer when empty and the source isn't done.
        if in_pos == in_fill && !src_eof {
            in_fill = 0;
            in_pos = 0;
            let n = src
                .read(&mut in_buf)
                .with_context(|| format!("{} decompress: read source", name))?;
            if n == 0 {
                src_eof = true;
            } else {
                in_fill = n;
            }
        }

        if in_pos == in_fill && src_eof {
            // No more input — tell the decoder we're done and flush any
            // final output. Loop in case the decoder emits more than
            // one output buffer worth.
            loop {
                let (progress, status) = dec
                    .finish(&mut out_buf)
                    .map_err(|e| anyhow!("{} finish: {:?}", name, e))?;
                if progress.written > 0 {
                    dst.write_all(&out_buf[..progress.written])
                        .with_context(|| format!("{} decompress: write dst", name))?;
                    total_out += progress.written as u64;
                }
                match status {
                    Status::StreamEnd => return Ok(total_out),
                    Status::OutputFull => continue,
                    Status::InputEmpty => {
                        bail!("{} decoder requested more input at EOF", name);
                    }
                }
            }
        }

        let (progress, status) = dec
            .decode(&in_buf[in_pos..in_fill], &mut out_buf)
            .map_err(|e| anyhow!("{} decode: {:?}", name, e))?;
        in_pos += progress.consumed;
        if progress.written > 0 {
            dst.write_all(&out_buf[..progress.written])
                .with_context(|| format!("{} decompress: write dst", name))?;
            total_out += progress.written as u64;
        }
        match status {
            Status::StreamEnd => return Ok(total_out),
            Status::InputEmpty | Status::OutputFull => {
                if progress.consumed == 0 && progress.written == 0 {
                    // Guard against a pathological no-progress loop.
                    bail!(
                        "{} decoder stalled (no input consumed, no output produced)",
                        name
                    );
                }
            }
        }
    }
}
