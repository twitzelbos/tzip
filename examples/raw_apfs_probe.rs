//! Probe that opens a raw block device and asks the vendored `apfs` crate
//! to parse it. Prints what it finds or the exact error it hits.
//!
//! Build:  cargo build --example raw_apfs_probe --features raw-apfs --release
//! Run:    sudo ./target/release/examples/raw_apfs_probe /dev/rdisk3
//!         sudo ./target/release/examples/raw_apfs_probe /dev/rdisk5 /telus_studies/exam_summary.csv
//!         sudo ./target/release/examples/raw_apfs_probe /dev/rdisk5 /telus_studies /some/big/file.dcm
//!
//! On Apple Silicon internal storage or any FileVault-enabled volume,
//! catalog reads fail at "invalid checksum" — see docs/RAW_BLOCK.md.
//! Plain external unencrypted APFS volumes should succeed end-to-end.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;

use apfs::ApfsVolume;

/// Read+Seek adapter over a macOS character-device file (`/dev/rdiskN`)
/// that satisfies the device's alignment rules: every read to the
/// underlying fd is at a block-aligned offset and in a block-aligned
/// length. The caller sees a regular byte-addressable stream.
///
/// No persistent buffer — each `read` computes the aligned range
/// covering exactly the caller's request, does one pread, hands out
/// the requested bytes. This keeps small B-tree-page reads cheap
/// (~4 KB round trip) while still supporting large extent reads.
///
/// Without this, callers that read the tail of a file (18338 bytes ≠
/// a multiple of 4096) hit EINVAL from `pread` on the raw device.
struct AlignedRawReader {
    file: File,
    pos: u64,
    block: u64,       // logical block size (4096 on APFS)
    scratch: Vec<u8>, // reused for the aligned pread
}

impl AlignedRawReader {
    fn new(file: File, block: u64) -> Self {
        Self { file, pos: 0, block, scratch: Vec::new() }
    }
}

impl Read for AlignedRawReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let want = out.len();
        if want == 0 {
            return Ok(0);
        }
        let start = self.pos;
        let end = start.saturating_add(want as u64);
        let aligned_start = start & !(self.block - 1);
        let aligned_end = end
            .checked_add(self.block - 1)
            .map(|v| v & !(self.block - 1))
            .unwrap_or(end);
        let aligned_len = (aligned_end - aligned_start) as usize;

        if self.scratch.len() < aligned_len {
            self.scratch.resize(aligned_len, 0);
        }
        self.file.seek(SeekFrom::Start(aligned_start))?;
        let mut got = 0;
        while got < aligned_len {
            match self.file.read(&mut self.scratch[got..aligned_len]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let off = (start - aligned_start) as usize;
        let avail = got.saturating_sub(off);
        let n = want.min(avail);
        out[..n].copy_from_slice(&self.scratch[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for AlignedRawReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(o) => o,
            SeekFrom::Current(d) => (self.pos as i64 + d) as u64,
            SeekFrom::End(_) => {
                let n = self.file.seek(pos)?;
                self.pos = n;
                return Ok(n);
            }
        };
        self.pos = new_pos;
        Ok(new_pos)
    }
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let dev = match args.next() {
        Some(d) => d,
        None => {
            eprintln!("usage: raw_apfs_probe /dev/rdiskN [dir_to_list] [file_to_read ...]");
            return ExitCode::from(2);
        }
    };
    let dir_to_list = args.next().unwrap_or_else(|| "/".to_string());
    let files_to_read: Vec<String> = args.collect();

    println!("== raw_apfs_probe ==");
    println!("device: {dev}");

    let started = std::time::Instant::now();
    let f = match OpenOptions::new().read(true).open(&dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("open({dev}) failed: {e}");
            eprintln!("  hint: raw devices are root:operator 0640 — need sudo or `operator` group");
            return ExitCode::from(1);
        }
    };
    println!("open: OK in {:.2}ms", started.elapsed().as_secs_f64() * 1000.0);

    // Wrap the raw device: aligns each read to the disk's block size.
    // No persistent buffer — one aligned pread per read, sized to the
    // caller's request. APFS's block size is 4096.
    let f = AlignedRawReader::new(f, 4096);

    let started = std::time::Instant::now();
    let mut vol = match ApfsVolume::open(f) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ApfsVolume::open failed: {e}");
            eprintln!("  possible causes:");
            eprintln!("    - device is not an APFS container (wrong partition?)");
            eprintln!("    - Apple Silicon internal storage returns ciphertext at the block layer");
            eprintln!("    - FileVault or T2 encryption on an external drive");
            eprintln!("    - `apfs` crate hit an edge case not covered yet");
            return ExitCode::from(3);
        }
    };
    println!("APFS parse: OK in {:.2}s", started.elapsed().as_secs_f64());

    let info = vol.volume_info().clone();
    println!("\n=== volume ===");
    println!("  name:          {}", info.name);
    println!("  block_size:    {}", info.block_size);
    println!("  num_files:     {}", info.num_files);
    println!("  num_dirs:      {}", info.num_directories);
    println!("  num_symlinks:  {}", info.num_symlinks);

    let started = std::time::Instant::now();
    match vol.list_directory(&dir_to_list) {
        Ok(entries) => {
            println!(
                "\n=== {} (top {} of {} entries) ===",
                dir_to_list,
                entries.len().min(10),
                entries.len()
            );
            println!(
                "  list_directory took {:.2}ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
            for e in entries.iter().take(10) {
                println!("  {:?}  {}  {} bytes", e.kind, e.name, e.size);
            }
        }
        Err(e) => {
            eprintln!("list_directory({dir_to_list}) failed: {e}");
            return ExitCode::from(4);
        }
    }

    for path in &files_to_read {
        // Special: if the arg is a directory, sweep the first N files in it
        // and report per-file timing distribution — closer to what tzip does.
        if let Ok(entries) = vol.list_directory(path) {
            let files: Vec<_> = entries
                .iter()
                .filter(|e| matches!(e.kind, apfs::EntryKind::File))
                .take(200)
                .collect();
            if !files.is_empty() {
                println!("\n=== timing sweep: {} first files in {} ===", files.len(), path);
                let mut times_us: Vec<u128> = Vec::with_capacity(files.len());
                let mut total_bytes: u64 = 0;
                let sweep_start = std::time::Instant::now();
                for e in &files {
                    let full = if path.ends_with('/') {
                        format!("{}{}", path, e.name)
                    } else {
                        format!("{}/{}", path, e.name)
                    };
                    let t = std::time::Instant::now();
                    match vol.read_file(&full) {
                        Ok(b) => {
                            total_bytes += b.len() as u64;
                            times_us.push(t.elapsed().as_micros());
                        }
                        Err(err) => {
                            eprintln!("  read({full}) failed: {err}");
                        }
                    }
                }
                let sweep = sweep_start.elapsed().as_secs_f64();
                times_us.sort();
                let median = times_us[times_us.len() / 2];
                let p95 = times_us[times_us.len() * 95 / 100];
                let max = times_us.last().copied().unwrap_or(0);
                let mbps = (total_bytes as f64 / 1_048_576.0) / sweep.max(0.000001);
                let files_per_sec = files.len() as f64 / sweep.max(0.000001);
                println!(
                    "  {} files, {} bytes, {:.2}s total",
                    files.len(),
                    total_bytes,
                    sweep
                );
                println!(
                    "  per-file: median {}μs  p95 {}μs  max {}μs",
                    median, p95, max
                );
                println!(
                    "  throughput: {:.1} files/sec, {:.1} MB/s aggregate",
                    files_per_sec, mbps
                );
                continue;
            }
        }
        match vol.stat(path) {
            Ok(s) => {
                println!("\n=== stat {path} ===");
                println!("  size: {} bytes  kind: {:?}", s.size, s.kind);
                let read_start = std::time::Instant::now();
                match vol.read_file(path) {
                    Ok(bytes) => {
                        let elapsed = read_start.elapsed().as_secs_f64();
                        let mbps = (bytes.len() as f64 / 1_048_576.0) / elapsed.max(0.000001);
                        println!(
                            "  read_file: {} bytes in {:.2}ms ({:.1} MB/s)",
                            bytes.len(),
                            elapsed * 1000.0,
                            mbps
                        );
                    }
                    Err(e) => eprintln!("  read_file failed: {e}"),
                }
            }
            Err(e) => eprintln!("stat({path}) failed: {e}"),
        }
    }

    println!("\nprobe: done");
    ExitCode::from(0)
}
