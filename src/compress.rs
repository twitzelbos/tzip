//! Compression backends for every method the ZIP format accepts.

use anyhow::{anyhow, Result};
use bzip2::write::BzEncoder;
use bzip2::Compression as BzCompression;
use libdeflater::{CompressionLvl, Compressor};
use rayon::prelude::*;
use std::io::Write;
use xz2::stream::{LzmaOptions, Stream};

use crate::cli::Method;

/// Output of a single-shot compression call.
///
/// `gp_bit1` — whether the ZIP general-purpose bit 1 must be set for this
/// entry. True for LZMA (signals EOS marker present).
pub struct CompressOut {
    pub data: Vec<u8>,
    pub gp_bit1: bool,
}

/// Per-worker scratch state. Reusing these buffers across items eliminates
/// hundreds of megabytes of allocator traffic on 500K-file corpora.
///
/// * `out` — output buffer for a compressed entry. Sized to
///   `deflate_compress_bound(input_len)` on demand; capacity never shrinks.
/// * `deflate` — libdeflater compressor. Cheap to reuse; free to recreate.
pub struct Scratch {
    pub out: Vec<u8>,
    pub deflate: Option<Compressor>,
    pub deflate_level: i32,
}

impl Scratch {
    pub fn new() -> Self {
        Self { out: Vec::new(), deflate: None, deflate_level: -1 }
    }

    fn get_deflate(&mut self, level: u32) -> Result<&mut Compressor> {
        let lvl = level.clamp(0, 12) as i32;
        if self.deflate.is_none() || self.deflate_level != lvl {
            let cl = CompressionLvl::new(lvl).map_err(|e| anyhow!("bad deflate level: {e:?}"))?;
            self.deflate = Some(Compressor::new(cl));
            self.deflate_level = lvl;
        }
        Ok(self.deflate.as_mut().unwrap())
    }
}

/// Compress `input` into a fresh `Vec` reusing `scratch.out` as a growing arena.
///
/// The returned `CompressOut.data` steals the scratch buffer; the caller
/// receives the compressed bytes and the scratch's `out` is reset to `Vec::new()`.
/// This is faster than cloning and keeps ownership clean for the writer.
pub fn compress(
    method: Method,
    level: u32,
    input: &[u8],
    scratch: &mut Scratch,
) -> Result<CompressOut> {
    match method {
        Method::Store => {
            // Fast path: no compression, just steal the input.
            let mut buf = std::mem::take(&mut scratch.out);
            buf.clear();
            buf.reserve(input.len());
            buf.extend_from_slice(input);
            Ok(CompressOut { data: buf, gp_bit1: false })
        }
        Method::Deflate => {
            let mut buf = std::mem::take(&mut scratch.out);
            let bound;
            {
                let c = scratch.get_deflate(level)?;
                bound = c.deflate_compress_bound(input.len());
            }
            if buf.capacity() < bound {
                buf.reserve(bound - buf.capacity());
            }
            // SAFETY: libdeflater writes exactly `n` bytes; we truncate to `n`
            // before exposing the buffer.
            unsafe { buf.set_len(bound); }
            let c = scratch.get_deflate(level)?;
            let n = c
                .deflate_compress(input, &mut buf[..])
                .map_err(|e| anyhow!("deflate failed: {e:?}"))?;
            buf.truncate(n);
            Ok(CompressOut { data: buf, gp_bit1: false })
        }
        Method::Bzip2 => bzip2(input, level).map(|d| CompressOut { data: d, gp_bit1: false }),
        Method::Lzma => lzma_for_zip(input, level).map(|d| CompressOut { data: d, gp_bit1: true }),
        Method::Xz => xz(input, level).map(|d| CompressOut { data: d, gp_bit1: false }),
        Method::Zstd => zstd(input, level).map(|d| CompressOut { data: d, gp_bit1: false }),
        Method::Ppmd => ppmd(input, level).map(|d| CompressOut { data: d, gp_bit1: false }),
    }
}

/// PPMd Ir1 (ZIP method 98) — encoder via `ppmd-rust`. Produces a
/// 2-byte little-endian ZIP PPMd header followed by the raw PPMd8
/// range-coded stream.
///
/// ZIP PPMd parameter header layout (per PKWARE APPNOTE §4.4.5.12,
/// matches `compcol::ppmd::ZipDecoder`):
///
/// ```text
/// bits 0..=3   model order - 1     (order 2..=16)
/// bits 4..=11  memory size MB - 1  (1..=256 MB)
/// bits 12..=15 restore method      (0 restart / 1 cut-off)
/// ```
///
/// Level tiers trade model arena (and therefore compression time +
/// RAM per concurrent entry) for ratio. Defaults land on a strong-but-
/// sane middle; level 9 maxes the arena out to 192 MB and order to 16.
fn ppmd(input: &[u8], level: u32) -> Result<Vec<u8>> {
    use ppmd_rust::{Ppmd8Encoder, RestoreMethod};

    let (order, mem_mb) = ppmd_params_for_level(level);
    let restore = RestoreMethod::Restart;

    // Pack the 2-byte header. The decoder stores `order - 1` and
    // `mem_mb - 1`, so a declared order of 16 → nibble 15, max mem
    // 256 → byte 255.
    let header: u16 = ((order - 1) as u16 & 0xF)
        | (((mem_mb - 1) as u16 & 0xFF) << 4)
        | ((restore_code(restore) as u16 & 0xF) << 12);
    let mut out: Vec<u8> = Vec::with_capacity(16 + input.len() / 4);
    out.extend_from_slice(&header.to_le_bytes());

    // Compress. `finish(true)` writes the PPMd end marker. ZIP carries
    // uncomp_size out of band, so a decoder *could* stop on size
    // alone — but 7-Zip's PPMd decoder validates the stream to its
    // end and errors without the marker, so we always emit it. Costs
    // a handful of bytes per entry.
    let enc = Ppmd8Encoder::new(out, order, (mem_mb as u32) * 1024 * 1024, restore)
        .map_err(|e| anyhow!("ppmd encoder init (order={order}, mem={mem_mb}MB): {e:?}"))?;
    let mut enc = enc;
    use std::io::Write as _;
    enc.write_all(input)
        .map_err(|e| anyhow!("ppmd write: {e:?}"))?;
    let finished = enc
        .finish(true)
        .map_err(|e| anyhow!("ppmd finish: {e:?}"))?;
    Ok(finished)
}

fn restore_code(m: ppmd_rust::RestoreMethod) -> u8 {
    use ppmd_rust::RestoreMethod::*;
    match m {
        Restart => 0,
        CutOff => 1,
        // ZIP PPMd per APPNOTE + 7-Zip rejects the freeze variant
        // ("Unsupported" in both compcol's decoder and 7-Zip's).
        // We never request it via `ppmd_params_for_level`, so this
        // arm is defensive.
        _ => 1,
    }
}

/// Map a tzip `--level` (1..=9) to `(model_order, memory_mb)`.
/// Defaults (level 6) pick a strong-but-modest tier: good ratio on
/// text, ~96 MB arena per concurrent entry. Level 9 maxes out the
/// ZIP format's arena cap (256 MB) and order (16).
fn ppmd_params_for_level(level: u32) -> (u32, u32) {
    let l = level.clamp(1, 9);
    match l {
        1 => (4, 4),
        2 => (5, 8),
        3 => (6, 16),
        4 => (8, 32),
        5 => (10, 48),
        6 => (12, 96),
        7 => (14, 128),
        8 => (15, 192),
        _ => (16, 192),
    }
}

/// Parallel-block DEFLATE, chunked ~1 MiB per block using flate2 with
/// `Z_SYNC_FLUSH` between chunks. Concatenating raw deflate streams that
/// were each terminated with a sync-flush marker (`0x00 0x00 0x00 0xff 0xff`)
/// is a valid DEFLATE stream — pigz uses the same trick.
///
/// Only used when a single file is large enough that per-file parallelism
/// alone wouldn't saturate cores (`size >= parallel_deflate_threshold`).
pub const PARALLEL_DEFLATE_THRESHOLD: u64 = 8 * 1024 * 1024;

pub fn compress_parallel_deflate(input: &[u8], level: u32) -> Result<CompressOut> {
    use flate2::{Compress, Compression, FlushCompress};

    const CHUNK: usize = 1 * 1024 * 1024;
    let level = level.clamp(0, 9);
    let chunks: Vec<&[u8]> = input.chunks(CHUNK).collect();
    let n_chunks = chunks.len();

    let parts: Vec<Result<Vec<u8>>> = chunks
        .par_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut z = Compress::new(Compression::new(level), false);
            let bound = chunk.len() + chunk.len() / 100 + 128;
            let mut out = Vec::with_capacity(bound);
            let is_last = i + 1 == n_chunks;
            let flush = if is_last { FlushCompress::Finish } else { FlushCompress::Sync };
            let mut in_pos = 0;
            loop {
                let before_in = z.total_in();
                let before_out = z.total_out();
                unsafe {
                    let cap = out.capacity();
                    out.set_len(cap);
                }
                let status = z
                    .compress(&chunk[in_pos..], &mut out[before_out as usize..], flush)
                    .map_err(|e| anyhow!("deflate chunk failed: {e:?}"))?;
                let produced = (z.total_out() - before_out) as usize;
                let consumed = (z.total_in() - before_in) as usize;
                in_pos += consumed;
                out.truncate(before_out as usize + produced);
                if matches!(status, flate2::Status::StreamEnd) {
                    break;
                }
                if in_pos >= chunk.len() && matches!(status, flate2::Status::Ok) && !is_last {
                    break;
                }
                if out.capacity() == out.len() {
                    let extra = (chunk.len() / 4).max(4096);
                    out.reserve(extra);
                }
            }
            Ok(out)
        })
        .collect();

    let mut total_len = 0;
    let parts: Vec<Vec<u8>> = parts.into_iter().collect::<Result<Vec<_>>>()?;
    for p in &parts {
        total_len += p.len();
    }
    let mut concat = Vec::with_capacity(total_len);
    for p in parts {
        concat.extend_from_slice(&p);
    }
    Ok(CompressOut { data: concat, gp_bit1: false })
}

pub fn method_id(method: Method) -> u16 {
    match method {
        Method::Store => 0,
        Method::Deflate => 8,
        Method::Bzip2 => 12,
        Method::Lzma => 14,
        Method::Zstd => 93,
        Method::Xz => 95,
        Method::Ppmd => 98,
    }
}

/// Minimum "version needed to extract" for each method, per APPNOTE.TXT.
pub fn version_needed(method: Method) -> u16 {
    match method {
        Method::Store | Method::Deflate => 20,
        Method::Bzip2 => 46,
        Method::Lzma => 63,
        Method::Zstd => 63,
        Method::Xz => 63,
        // APPNOTE §4.4.3.2: method 98 (PPMd) needs v6.3 to extract.
        Method::Ppmd => 63,
    }
}

fn bzip2(input: &[u8], level: u32) -> Result<Vec<u8>> {
    let lvl = level.clamp(1, 9);
    let mut enc = BzEncoder::new(Vec::new(), BzCompression::new(lvl));
    enc.write_all(input)?;
    Ok(enc.finish()?)
}

fn xz(input: &[u8], level: u32) -> Result<Vec<u8>> {
    let lvl = level.clamp(0, 9);
    let mut out = Vec::with_capacity(input.len() / 2);
    let mut enc = xz2::write::XzEncoder::new(&mut out, lvl);
    enc.write_all(input)?;
    enc.finish()?;
    Ok(out)
}

fn zstd(input: &[u8], level: u32) -> Result<Vec<u8>> {
    let lvl = level.clamp(1, 22) as i32;
    let out = zstd::stream::encode_all(input, lvl)?;
    Ok(out)
}

/// LZMA-in-ZIP wrapper (method 14).
///
/// ZIP method 14 file-data layout:
///
///     [ major (1) | minor (1) | props_len (2, LE) | LZMA1 props (N) | LZMA1 data ]
///
/// where props_len is 5 (properties byte + dict size LE). `xz2` emits an
/// LZMA_ALONE stream: `[props(1) | dict(4) | uncomp_size(8) | data...]` — we
/// slice off the 8-byte size field and reassemble in the ZIP order.
fn lzma_for_zip(input: &[u8], level: u32) -> Result<Vec<u8>> {
    let level = level.clamp(0, 9);
    let opts = LzmaOptions::new_preset(level).map_err(|e| anyhow!("lzma opts: {e:?}"))?;
    let stream = Stream::new_lzma_encoder(&opts).map_err(|e| anyhow!("lzma encoder: {e:?}"))?;
    let mut enc = xz2::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(input)?;
    let alone = enc.finish()?;

    if alone.len() < 13 {
        return Err(anyhow!("lzma stream too short: {} bytes", alone.len()));
    }
    let props = &alone[0..5]; // props byte + dict size
    let data = &alone[13..]; // skip 8-byte uncompressed-size field
    let mut out = Vec::with_capacity(4 + 5 + data.len());
    // major/minor version bytes: match LZMA SDK 19.00 (0x13, 0x00) — most
    // unarchivers accept anything here.
    out.push(0x13);
    out.push(0x00);
    // properties length (5, LE u16)
    out.push(0x05);
    out.push(0x00);
    out.extend_from_slice(props);
    out.extend_from_slice(data);
    Ok(out)
}
