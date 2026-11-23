//! Streaming decompression — the inverse of `compress.rs`. Each method
//! dispatches to a `Read`-adapter decoder so a multi-GB entry doesn't need
//! to be fully buffered.
//!
//! LZMA (ZIP method 14) is the one special case: ZIP method 14 wraps the
//! LZMA1 stream in a custom 4-byte prefix (`major`, `minor`,
//! `props_len_le_u16`) that is NOT part of the LZMA_ALONE format. On the
//! decode side we strip that prefix, re-assemble a well-formed LZMA_ALONE
//! stream with a known uncompressed size, and feed xz2's LZMA decoder.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};

use crate::cli::Method;

pub fn decompress<R: Read, W: Write>(
    method: Method,
    src: R,
    uncomp_size: u64,
    dst: &mut W,
) -> Result<u64> {
    match method {
        Method::Store => copy_exact(src, dst, uncomp_size).context("store copy"),
        Method::Deflate => {
            let mut dec = flate2::read::DeflateDecoder::new(src);
            std::io::copy(&mut dec, dst).context("deflate decompress")
        }
        Method::Bzip2 => {
            let mut dec = bzip2::read::BzDecoder::new(src);
            std::io::copy(&mut dec, dst).context("bzip2 decompress")
        }
        Method::Xz => {
            let mut dec = xz2::read::XzDecoder::new(src);
            std::io::copy(&mut dec, dst).context("xz decompress")
        }
        Method::Zstd => {
            let mut dec =
                zstd::stream::read::Decoder::new(src).context("zstd decoder init")?;
            std::io::copy(&mut dec, dst).context("zstd decompress")
        }
        Method::Lzma => decompress_lzma(src, uncomp_size, dst),
    }
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
