//! Contention warner (macOS).
//!
//! Detects the two most common reasons tzip's throughput craters on the
//! same drive it "should" saturate:
//!
//! 1. Spotlight indexing enabled on the mount — every write / new file
//!    triggers `mds_stores` and `mdworker*` to read the file back for
//!    indexing, doubling drive traffic and adding synchronous latency.
//! 2. On-access antivirus scanners (Sophos, CrowdStrike, McAfee, Symantec,
//!    ESET, MalwareBytes, SentinelOne, Carbon Black, Trend Micro) that
//!    intercept every `open()` and synchronously scan the file before
//!    returning the fd. This can multiply per-file overhead by 5-10×.
//!
//! Run once at pipeline startup; emit warnings with actionable
//! suggestions. Never fail the run.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct ContentionReport {
    pub spotlight_mounts: Vec<PathBuf>,
    pub scanners: Vec<ScannerProc>,
    pub apple_indexers: Vec<ScannerProc>,
}

#[allow(dead_code)]
pub struct ScannerProc {
    pub name: String,
    pub pid: u32,
    pub cpu_pct: f32,
    pub vendor: &'static str,
}

/// Known third-party security-scanner process names, with the vendor tag
/// used in warnings. Match is a substring test on the process's argv[0]
/// basename (lowercased).
const SCANNER_SIGNATURES: &[(&str, &str)] = &[
    // Sophos
    ("sophos", "Sophos"),
    // CrowdStrike
    ("falcond", "CrowdStrike"),
    ("com.crowdstrike.falcon", "CrowdStrike"),
    // McAfee / Trellix
    ("mfeatp", "McAfee/Trellix"),
    ("mfeavscan", "McAfee/Trellix"),
    ("mcafeeagent", "McAfee/Trellix"),
    // Symantec / Broadcom
    ("symdaemon", "Symantec"),
    ("symcfgd", "Symantec"),
    ("symuiagent", "Symantec"),
    // ESET
    ("esets_daemon", "ESET"),
    ("ec_ss", "ESET"),
    // Malwarebytes
    ("rtprotectiondaemon", "Malwarebytes"),
    ("malwarebytes", "Malwarebytes"),
    // SentinelOne
    ("sentineld", "SentinelOne"),
    ("sentinelagent", "SentinelOne"),
    ("com.sentinelone", "SentinelOne"),
    // Carbon Black
    ("cbdefense", "Carbon Black"),
    ("cbdaemon", "Carbon Black"),
    ("repmgr", "Carbon Black"),
    // Trend Micro
    ("icoreservice", "Trend Micro"),
    ("trend micro", "Trend Micro"),
    // Elastic Endpoint
    ("elastic-endpoint", "Elastic"),
    // Bitdefender
    ("bdlaunchd", "Bitdefender"),
    ("bdredline", "Bitdefender"),
];

/// Apple-internal indexers / snapshotters that also compete for drive I/O.
/// These are more likely to be tolerable but worth surfacing when they're
/// hot.
const APPLE_SIGNATURES: &[(&str, &str)] = &[
    ("mds_stores", "Spotlight"),
    ("mdworker", "Spotlight"),
    ("mds", "Spotlight"),
    ("fseventsd", "FSEvents"),
    ("revisiond", "Versions/Snapshots"),
    ("backuploader", "Time Machine"),
    ("backupd", "Time Machine"),
];

/// CPU threshold for including a matched process in the report.
/// Anything below this is considered idle background noise.
const CPU_THRESHOLD_PCT: f32 = 5.0;

/// Probe all mounts touched by the workload and the running process list.
pub fn probe(mounts: &[PathBuf]) -> ContentionReport {
    let spotlight_mounts = probe_spotlight(mounts);
    let (scanners, apple_indexers) = probe_processes();
    ContentionReport { spotlight_mounts, scanners, apple_indexers }
}

pub fn print_warnings(report: &ContentionReport) {
    if report.spotlight_mounts.is_empty()
        && report.scanners.is_empty()
        && report.apple_indexers.is_empty()
    {
        return;
    }
    eprintln!();
    eprintln!("=== contention check ===");

    for mount in &report.spotlight_mounts {
        eprintln!(
            "warning: Spotlight indexing is ENABLED on {}",
            mount.display()
        );
        eprintln!(
            "  → to disable:  sudo mdutil -i off {}",
            mount.display()
        );
        eprintln!(
            "  → to also drop the existing index:  sudo mdutil -E {}",
            mount.display()
        );
    }

    if !report.scanners.is_empty() {
        // Deduplicate by vendor for the summary
        let mut by_vendor: std::collections::BTreeMap<&str, f32> = Default::default();
        for s in &report.scanners {
            *by_vendor.entry(s.vendor).or_insert(0.0) += s.cpu_pct;
        }
        eprintln!("warning: on-access AV scanner(s) actively consuming CPU:");
        for (vendor, cpu) in &by_vendor {
            eprintln!("  → {} ({:.0}% cumulative CPU across matched processes)", vendor, cpu);
        }
        eprintln!(
            "  → AV scanners intercept every open() and synchronously scan the file;"
        );
        eprintln!(
            "    expect 3-10× per-file overhead. Ask IT to exclude your source drive(s)"
        );
        eprintln!("    from on-access scanning, or temporarily pause the agent for the run.");
    }

    if !report.apple_indexers.is_empty() {
        let mut by_vendor: std::collections::BTreeMap<&str, f32> = Default::default();
        for s in &report.apple_indexers {
            *by_vendor.entry(s.vendor).or_insert(0.0) += s.cpu_pct;
        }
        eprintln!("note: Apple indexer(s) also active:");
        for (vendor, cpu) in &by_vendor {
            eprintln!("  → {} ({:.0}% cumulative CPU)", vendor, cpu);
        }
    }

    eprintln!("=== end contention check ===");
    eprintln!();
}

fn probe_spotlight(mounts: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: std::collections::HashSet<PathBuf> = Default::default();
    let mut out = Vec::new();
    for m in mounts {
        if !seen.insert(m.clone()) {
            continue;
        }
        // `mdutil -s <path>` prints "Indexing enabled." / "Indexing disabled." /
        // "No index." Anything containing "enabled" (case-insensitive) is on.
        let Ok(o) = Command::new("mdutil").arg("-s").arg(m).output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&o.stdout).to_lowercase();
        if text.contains("indexing enabled") {
            out.push(m.clone());
        }
    }
    out
}

fn probe_processes() -> (Vec<ScannerProc>, Vec<ScannerProc>) {
    let mut scanners = Vec::new();
    let mut apple = Vec::new();

    // `ps -Ao pid,%cpu,command` gives us one line per process. `command`
    // is the full argv joined by spaces; we lowercase and substring-match.
    let Ok(out) = Command::new("ps")
        .arg("-Ao")
        .arg("pid,%cpu,command")
        .output()
    else {
        return (scanners, apple);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().skip(1) {
        let trimmed = line.trim_start();
        // `ps -Ao pid,%cpu,command` pads with variable whitespace. Take the
        // first two whitespace-separated tokens (pid, %cpu) and treat the
        // rest of the line as the full command — which may itself contain
        // spaces (e.g. `/Library/Sophos Anti-Virus/...`).
        let mut ws = trimmed.split_whitespace();
        let pid = match ws.next().and_then(|s| s.parse::<u32>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let cpu = match ws.next().and_then(|s| s.parse::<f32>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let command_rest: Vec<&str> = ws.collect();
        if command_rest.is_empty() {
            continue;
        }
        let command = command_rest.join(" ");
        let command = command.as_str();
        if cpu < CPU_THRESHOLD_PCT {
            continue;
        }
        let cmd_lower = command.to_ascii_lowercase();

        for (needle, vendor) in SCANNER_SIGNATURES {
            if cmd_lower.contains(needle) {
                let name = short_name(command);
                scanners.push(ScannerProc { name, pid, cpu_pct: cpu, vendor });
                break;
            }
        }
        for (needle, vendor) in APPLE_SIGNATURES {
            if cmd_lower.contains(needle) {
                let name = short_name(command);
                apple.push(ScannerProc { name, pid, cpu_pct: cpu, vendor });
                break;
            }
        }
    }
    (scanners, apple)
}

fn short_name(command: &str) -> String {
    // Take the basename of argv[0] for a compact display.
    let argv0 = command.split_whitespace().next().unwrap_or(command);
    Path::new(argv0)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| argv0.to_string())
}
