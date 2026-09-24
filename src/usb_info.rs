//! macOS USB / drive diagnostic (`--usb-info`).
//!
//! For each source path, prints:
//!   • mount point + filesystem type
//!   • BSD device (e.g. `/dev/disk7s1`) + Bus Protocol
//!   • USB link speed and product / vendor when the source is on USB
//!   • advisories (APFS-on-USB, USB 2.0 fallback, wrong reader-jobs setting)
//!
//! Implementation is intentionally shell-based — parsing `diskutil` and
//! `ioreg` text output is quick to write, quick to modify, and doesn't
//! require IOKit FFI. If we need lower latency later we can swap in a
//! CoreFoundation/IOKit path.

#![cfg(target_os = "macos")]

use anyhow::{anyhow, Result};
use std::ffi::CStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct SourceReport {
    #[allow(dead_code)]
    pub path: PathBuf,
    pub mount_point: PathBuf,
    pub bsd_device: String,
    pub fs_type: String,
    pub bus_protocol: Option<String>,
    pub media_name: Option<String>,
    pub usb: Option<UsbLinkInfo>,
}

pub struct UsbLinkInfo {
    pub vendor: String,
    pub product: String,
    pub link_speed_bps: u64,
}

impl UsbLinkInfo {
    pub fn speed_label(&self) -> String {
        match self.link_speed_bps {
            0 => "unknown".into(),
            n if n >= 20_000_000_000 => format!("USB 3.2 Gen 2x2 (~{} Gbps)", n / 1_000_000_000),
            n if n >= 10_000_000_000 => format!("USB 3.1 SuperSpeed+ (~{} Gbps)", n / 1_000_000_000),
            n if n >= 5_000_000_000 => format!("USB 3.0 SuperSpeed (~{} Gbps)", n / 1_000_000_000),
            480_000_000 => "USB 2.0 High Speed (480 Mbps)".into(),
            12_000_000 => "USB 1.1 Full Speed (12 Mbps)".into(),
            1_500_000 => "USB 1.0 Low Speed (1.5 Mbps)".into(),
            n => format!("{} bps (unknown class)", n),
        }
    }
}

/// Probe every input path AND the output archive's parent directory.
///
/// Skips paths whose backing filesystem isn't on a USB bus — the diagnostic
/// is only meaningful when at least one leg of the archive touches USB.
/// Returns `Vec<SourceReport>` deduped by mount point, only including USB-backed
/// filesystems.
/// Public single-path probe. Callers that don't need the deduped `Vec`
/// (auto-tune, unit tests) can use this directly.
pub fn probe_path(path: &Path) -> Result<SourceReport> {
    probe_one(path)
}

pub fn probe_workload(inputs: &[PathBuf], output_archive: &Path) -> Vec<SourceReport> {
    let mut probe_paths: Vec<(PathBuf, &'static str)> = inputs
        .iter()
        .map(|p| (p.clone(), "input"))
        .collect();

    // Output goes to `output_archive`'s parent directory (the archive file
    // itself doesn't exist yet; statfs needs an existing path).
    let out_probe = output_archive
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    probe_paths.push((out_probe, "output"));

    let mut seen: std::collections::HashSet<PathBuf> = Default::default();
    let mut out = Vec::new();
    for (p, _role) in &probe_paths {
        match probe_one(p) {
            Ok(r) => {
                if r.bus_protocol.as_deref() != Some("USB") {
                    continue;
                }
                if seen.insert(r.mount_point.clone()) {
                    out.push(r);
                }
            }
            Err(e) => {
                eprintln!("usb-info: probe {} failed: {}", p.display(), e);
            }
        }
    }
    out
}

pub fn print_reports(reports: &[SourceReport], read_jobs: usize, keep_cache: bool) {
    println!();
    if reports.is_empty() {
        println!("=== USB / drive diagnostic ===");
        println!("no USB-backed filesystems in this workload — nothing to report.");
        println!("=== end diagnostic ===");
        println!();
        return;
    }
    println!("=== USB / drive diagnostic ===");
    for r in reports {
        println!();
        println!("mount         {}", r.mount_point.display());
        println!("  fs          {}", r.fs_type);
        println!("  bsd device  {}", r.bsd_device);
        if let Some(bp) = &r.bus_protocol {
            println!("  bus         {}", bp);
        }
        if let Some(mn) = &r.media_name {
            println!("  media       {}", mn);
        }
        if let Some(u) = &r.usb {
            println!("  usb device  {} {}", u.vendor, u.product);
            println!("  usb link    {}", u.speed_label());
        }
        for line in advise(r, read_jobs, keep_cache) {
            println!("  advisory    {}", line);
        }
    }
    println!();
    println!("=== end diagnostic ===");
    println!();
}

fn advise(r: &SourceReport, read_jobs: usize, keep_cache: bool) -> Vec<String> {
    let mut out = Vec::new();
    let is_usb = r.bus_protocol.as_deref() == Some("USB");
    let is_apfs = r.fs_type.eq_ignore_ascii_case("apfs");
    let is_exfat = r.fs_type.eq_ignore_ascii_case("exfat")
        || r.fs_type.eq_ignore_ascii_case("msdos");

    if is_usb && is_apfs {
        out.push(
            "APFS on USB is known-slow (many small metadata syscalls over USB-MSC). \
             Consider `--keep-cache --read-jobs 1 --dispatch-io`, or reformat to exfat."
                .into(),
        );
    }
    if is_usb {
        if let Some(u) = &r.usb {
            if u.link_speed_bps <= 480_000_000 {
                out.push(format!(
                    "USB link is {} — cap ~30 MB/s regardless of drive speed. \
                     Check the cable, port, and hub.",
                    u.speed_label()
                ));
            }
        }
        if read_jobs > 2 {
            out.push(format!(
                "--read-jobs {} is high for a USB source; concurrent readers can seek-storm.",
                read_jobs
            ));
        }
        if !keep_cache && is_apfs {
            out.push(
                "F_NOCACHE is on (default). On APFS-USB it kills readahead — pass --keep-cache."
                    .into(),
            );
        }
    } else if is_exfat && !keep_cache {
        // exfat over USB is usually fine with F_NOCACHE — no advisory needed
    }
    out
}

fn probe_one(path: &Path) -> Result<SourceReport> {
    // statfs on the source path
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("path contains NUL"))?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(path_c.as_ptr(), &mut sfs) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let fs_type = c_array_to_string(&sfs.f_fstypename);
    let mount_point = PathBuf::from(c_array_to_string(&sfs.f_mntonname));
    let bsd_device = c_array_to_string(&sfs.f_mntfromname);

    // diskutil info gives us Bus Protocol, Device / Media Name
    let (bus_protocol, media_name) = diskutil_info(&bsd_device).unwrap_or((None, None));

    // If USB, correlate to a USB device via ioreg
    let usb = if bus_protocol.as_deref() == Some("USB") {
        find_usb_device(media_name.as_deref()).ok().flatten()
    } else {
        None
    };

    Ok(SourceReport {
        path: path.to_path_buf(),
        mount_point,
        bsd_device,
        fs_type,
        bus_protocol,
        media_name,
        usb,
    })
}

fn c_array_to_string(arr: &[libc::c_char]) -> String {
    // Safety: arr is a NUL-terminated C string embedded in a fixed-size array.
    let cs = unsafe { CStr::from_ptr(arr.as_ptr()) };
    cs.to_string_lossy().into_owned()
}

fn diskutil_info(bsd_device: &str) -> Result<(Option<String>, Option<String>)> {
    let (bus, name, part_of_whole) = diskutil_info_raw(bsd_device)?;
    // Media name is often only set on the whole-disk entry, not the partition.
    // If we're looking at a partition (e.g. /dev/disk7s1) and got no name,
    // re-probe the whole disk (`/dev/disk7`) for the drive's product name.
    if name.is_none() {
        if let Some(whole) = part_of_whole {
            if !whole.is_empty() && whole != bsd_device.trim_start_matches("/dev/") {
                let whole_path = format!("/dev/{}", whole);
                if let Ok((bus2, name2, _)) = diskutil_info_raw(&whole_path) {
                    return Ok((bus.or(bus2), name2));
                }
            }
        }
    }
    Ok((bus, name))
}

/// Parse the fields we care about from `diskutil info <device>`.
/// Returns (bus_protocol, media_name, part_of_whole_dev_basename).
///
/// macOS Sonoma renamed `Bus Protocol:` to just `Protocol:`; we accept either.
fn diskutil_info_raw(
    bsd_device: &str,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let out = Command::new("diskutil").arg("info").arg(bsd_device).output()?;
    if !out.status.success() {
        return Ok((None, None, None));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut bus_protocol = None;
    let mut media_name = None;
    let mut part_of_whole = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Bus Protocol:") {
            bus_protocol = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Protocol:") {
            if bus_protocol.is_none() {
                bus_protocol = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("Device / Media Name:") {
            media_name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Media Name:") {
            if media_name.is_none() {
                media_name = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("Part of Whole:") {
            part_of_whole = Some(v.trim().to_string());
        }
    }
    Ok((bus_protocol, media_name, part_of_whole))
}

fn find_usb_device(media_name: Option<&str>) -> Result<Option<UsbLinkInfo>> {
    let out = Command::new("ioreg").arg("-p").arg("IOUSB").arg("-l").output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // ioreg output is a tree; each USB device begins with a header line like
    //   `+-o Extreme V3@01100000  <class ...>`
    // followed by indented `| "Key" = value` lines. We collect per-device blocks
    // and match by USB Product Name against `media_name` (which is usually the
    // product name as reported to `diskutil` too).
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.contains("+-o ") {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let want = media_name.map(|s| s.to_lowercase());

    // First pass: find a block whose USB Product Name matches media_name.
    // Second pass fallback: pick the first block that has a non-hub Product Name.
    let mut best_match: Option<UsbLinkInfo> = None;
    for block in &blocks {
        let product = extract_quoted(block, "USB Product Name").unwrap_or_default();
        let vendor = extract_quoted(block, "USB Vendor Name").unwrap_or_default();
        let speed = extract_number(block, "UsbLinkSpeed").unwrap_or(0);
        if product.is_empty() {
            continue;
        }
        if let Some(w) = &want {
            let p = product.to_lowercase();
            if p == *w || w.contains(&p) || p.contains(w) {
                return Ok(Some(UsbLinkInfo { vendor, product, link_speed_bps: speed }));
            }
        }
        // Track a plausible best-effort match: any non-hub, non-empty product.
        if !product.to_lowercase().contains("hub") && best_match.is_none() {
            best_match = Some(UsbLinkInfo { vendor, product, link_speed_bps: speed });
        }
    }
    Ok(best_match)
}

fn extract_quoted(block: &[&str], key: &str) -> Option<String> {
    let needle = format!("\"{}\" = \"", key);
    for line in block {
        if let Some(idx) = line.find(&needle) {
            let after = &line[idx + needle.len()..];
            if let Some(end) = after.find('"') {
                return Some(after[..end].to_string());
            }
        }
    }
    None
}

fn extract_number(block: &[&str], key: &str) -> Option<u64> {
    let needle = format!("\"{}\" = ", key);
    for line in block {
        if let Some(idx) = line.find(&needle) {
            let after = &line[idx + needle.len()..];
            let end = after
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after.len());
            if let Ok(n) = after[..end].parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}
