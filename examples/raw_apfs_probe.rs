//! Probe that opens a raw block device and asks the vendored `apfs` crate
//! to parse it. Prints what it finds or the exact error it hits.
//!
//! Build:  cargo build --example raw_apfs_probe --features raw-apfs --release
//! Run:    sudo ./target/release/examples/raw_apfs_probe /dev/rdisk3
//!
//! On Apple Silicon internal storage the parse may fail even with sudo —
//! that's the point of the probe. Encrypted external volumes (FileVault
//! on) also fail. Plain external APFS volumes should succeed.

use std::env;
use std::fs::OpenOptions;
use std::process::ExitCode;

use apfs::ApfsVolume;

fn main() -> ExitCode {
    let dev = match env::args().nth(1) {
        Some(d) => d,
        None => {
            eprintln!("usage: raw_apfs_probe /dev/rdiskN");
            return ExitCode::from(2);
        }
    };

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

    // Try listing the root
    let started = std::time::Instant::now();
    match vol.list_directory("/") {
        Ok(entries) => {
            println!(
                "\n=== root directory (top {} entries) ===",
                entries.len().min(10)
            );
            println!("  list_directory took {:.2}ms", started.elapsed().as_secs_f64() * 1000.0);
            for e in entries.iter().take(10) {
                println!("  {:?}  {}  {} bytes", e.kind, e.name, e.size);
            }
        }
        Err(e) => {
            eprintln!("list_directory(/) failed: {e}");
            return ExitCode::from(4);
        }
    }

    // Try reading a small well-known file
    for path in ["/exam_summary.csv", "/.metadata_never_index"] {
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
            Err(_) => {
                // File doesn't exist on this volume — normal
            }
        }
    }

    println!("\nprobe: done");
    ExitCode::from(0)
}
