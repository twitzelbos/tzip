//! `tzip bench` — reproducible timing of tzip against system `7z`,
//! `zip`, and `unzip` on a shared corpus.
//!
//! Three benchmarks are reported:
//!   - **create**: build an archive from the corpus.
//!   - **extract**: unpack that archive to a fresh tempdir.
//!   - **list**: enumerate archive entries.
//!
//! Each tool runs against its *own* archive (self-create → self-extract)
//! so you see the real end-to-end cost of using that tool, not just the
//! decompressor. Where output sizes differ, the size + ratio columns
//! surface that trade-off. Missing external tools are skipped and
//! reported; results stand on their own with just tzip when nothing
//! else is on PATH.
//!
//! The synthetic corpus is deterministic (seeded PRNG), mixing text
//! and incompressible binary files so compression ratios are
//! meaningful.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::BenchOptions;

pub fn run(opts: BenchOptions) -> Result<()> {
    let (corpus_dir, corpus_bytes, _corpus_guard) = resolve_corpus(opts.corpus.as_deref(), opts.quick)?;
    let tools = detect_tools();

    if !opts.quiet {
        eprintln!(
            "tzip bench: corpus {} ({} files, {:.1} MiB)",
            corpus_dir.display(),
            count_files(&corpus_dir),
            corpus_bytes as f64 / (1024.0 * 1024.0),
        );
        eprintln!(
            "tzip bench: iterations {}, external tools: {}",
            opts.iterations,
            tools.describe(),
        );
    }

    let only: std::collections::HashSet<String> = opts.only.iter().cloned().collect();
    let include = |name: &str| only.is_empty() || only.contains(name);

    // Scratch dir holds all tool-created archives + extraction targets
    // for the lifetime of the run.
    let scratch = tempfile::tempdir().context("create scratch tempdir")?;

    // Create always runs — it produces the archives extract and list
    // consume. `--only extract` / `--only list` still need those
    // archives to exist. The create row only appears in the output
    // when `create` is in the include set (or the set is empty).
    let create_results = bench_create(&corpus_dir, corpus_bytes, opts.iterations, &tools, scratch.path())?;

    let mut results: Vec<BenchResult> = Vec::new();
    if include("create") {
        results.extend(create_results);
    }
    if include("extract") {
        results.extend(bench_extract(opts.iterations, &tools, scratch.path())?);
    }
    if include("list") {
        results.extend(bench_list(opts.iterations, &tools, scratch.path())?);
    }

    if opts.json {
        for r in &results {
            println!("{}", serde_json::to_string(r)?);
        }
    } else {
        emit_table(&results);
    }
    Ok(())
}

// ── Synthetic corpus ───────────────────────────────────────────────────────

fn resolve_corpus(
    user: Option<&Path>,
    quick: bool,
) -> Result<(PathBuf, u64, Option<tempfile::TempDir>)> {
    if let Some(p) = user {
        let dir = p.to_path_buf();
        if !dir.is_dir() {
            bail!("--corpus {} is not a directory", dir.display());
        }
        let bytes = total_file_bytes(&dir)?;
        return Ok((dir, bytes, None));
    }
    let td = tempfile::tempdir().context("create corpus tempdir")?;
    let bytes = generate_synthetic_corpus(td.path(), quick)
        .context("generate synthetic corpus")?;
    let path = td.path().to_path_buf();
    Ok((path, bytes, Some(td)))
}

/// SplitMix64 — one-liner PRNG, no dep. Seeded for reproducibility.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i + 8 <= buf.len() {
            let x = self.next_u64().to_le_bytes();
            buf[i..i + 8].copy_from_slice(&x);
            i += 8;
        }
        if i < buf.len() {
            let tail = buf.len() - i;
            let x = self.next_u64().to_le_bytes();
            buf[i..].copy_from_slice(&x[..tail]);
        }
    }
}

fn generate_synthetic_corpus(root: &Path, quick: bool) -> Result<u64> {
    // Shape: text files (compressible) + random binary (incompressible)
    // + small structured JSON-like entries, across a nested directory
    // tree. Targets ~50 MiB default, ~5 MiB with --quick.
    const LOREM: &[u8] = b"The quick brown fox jumps over the lazy dog, \
        repeatedly, demonstrating that this phrase contains every letter \
        of the English alphabet. Repetition is the mother of compression. \n";

    let mut rng = SplitMix64::new(0xA5A5_5A5A_1234_5678);
    let mut total: u64 = 0;

    let (text_spec, binary_spec, json_spec): (&[(usize, usize)], &[(usize, usize)], &[(usize, usize)]) =
        if quick {
            (
                &[(8, 4 * 1024), (4, 64 * 1024), (1, 512 * 1024)],
                &[(2, 256 * 1024), (1, 1 * 1024 * 1024)],
                &[(16, 2 * 1024)],
            )
        } else {
            (
                &[(100, 4 * 1024), (40, 64 * 1024), (10, 512 * 1024), (2, 4 * 1024 * 1024)],
                &[(20, 256 * 1024), (8, 1024 * 1024), (3, 8 * 1024 * 1024)],
                &[(200, 2 * 1024), (40, 16 * 1024)],
            )
        };

    let mut file_idx = 0usize;
    let mut write_file = |rel_dir: &str, ext: &str, bytes: Vec<u8>, total: &mut u64| -> Result<()> {
        let dir = root.join(format!("{}/{:03}", rel_dir, file_idx / 50));
        fs::create_dir_all(&dir)?;
        let p = dir.join(format!("{:05}.{}", file_idx, ext));
        fs::write(&p, &bytes)?;
        *total += bytes.len() as u64;
        file_idx += 1;
        Ok(())
    };

    // Text files — repeated LOREM with rotation so compressors see
    // different starting positions.
    for &(count, size) in text_spec {
        for _ in 0..count {
            let mut body = Vec::with_capacity(size);
            while body.len() < size {
                let take = (size - body.len()).min(LOREM.len());
                body.extend_from_slice(&LOREM[..take]);
            }
            write_file("text", "txt", body, &mut total)?;
        }
    }
    // Random binary — incompressible payload.
    for &(count, size) in binary_spec {
        for _ in 0..count {
            let mut body = vec![0u8; size];
            rng.fill(&mut body);
            write_file("binary", "bin", body, &mut total)?;
        }
    }
    // JSON-like structured records — moderately compressible.
    for &(count, size) in json_spec {
        for _ in 0..count {
            let mut body = String::with_capacity(size);
            let mut i = 0u64;
            while body.len() < size {
                body.push_str(&format!(
                    "{{\"id\":{},\"name\":\"item_{:08}\",\"value\":{:.4},\"active\":true}}\n",
                    i,
                    i,
                    (rng.next_u64() % 100_000) as f64 / 100.0,
                ));
                i += 1;
            }
            body.truncate(size);
            write_file("json", "jsonl", body.into_bytes(), &mut total)?;
        }
    }

    Ok(total)
}

fn total_file_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    walk(dir, &mut |p| {
        if let Ok(m) = fs::metadata(p) {
            if m.is_file() {
                total += m.len();
            }
        }
    })?;
    Ok(total)
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0usize;
    let _ = walk(dir, &mut |p| {
        if p.is_file() {
            n += 1;
        }
    });
    n
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            walk(&p, f)?;
        } else {
            f(&p);
        }
    }
    Ok(())
}

// ── Tool detection ─────────────────────────────────────────────────────────

struct Tools {
    tzip: PathBuf, // our own binary — argv[0] self-reference
    seven_zip: Option<String>,
    zip: Option<String>,
    unzip: Option<String>,
}

impl Tools {
    fn describe(&self) -> String {
        let mut avail: Vec<&str> = vec!["tzip"];
        if self.seven_zip.is_some() {
            avail.push("7z");
        }
        if self.zip.is_some() {
            avail.push("zip");
        }
        if self.unzip.is_some() {
            avail.push("unzip");
        }
        avail.join(", ")
    }
}

fn detect_tools() -> Tools {
    Tools {
        tzip: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tzip")),
        seven_zip: which("7zz").or_else(|| which("7z")).or_else(|| which("7za")),
        zip: which("zip"),
        unzip: which("unzip"),
    }
}

fn which(cmd: &str) -> Option<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", cmd))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ── Benchmarks ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct BenchResult {
    benchmark: String,
    tool: String,
    iterations: usize,
    min_ms: u128,
    median_ms: u128,
    max_ms: u128,
    bytes: u64,
    throughput_mib_per_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ratio: Option<f64>,
}

fn time_cmd<F: FnMut() -> Result<()>>(
    mut f: F,
    iterations: usize,
) -> Result<Vec<Duration>> {
    let mut v = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f()?;
        v.push(start.elapsed());
    }
    Ok(v)
}

fn summarize(
    benchmark: &str,
    tool: &str,
    durations: &[Duration],
    bytes: u64,
    output_size: Option<u64>,
    input_bytes_for_ratio: u64,
) -> BenchResult {
    let mut sorted: Vec<u128> = durations.iter().map(|d| d.as_millis()).collect();
    sorted.sort();
    let min_ms = *sorted.first().unwrap();
    let max_ms = *sorted.last().unwrap();
    let median_ms = sorted[sorted.len() / 2];
    let throughput = if median_ms == 0 {
        0.0
    } else {
        (bytes as f64 / (1024.0 * 1024.0)) / (median_ms as f64 / 1000.0)
    };
    let ratio = output_size.and_then(|out| {
        if input_bytes_for_ratio == 0 {
            None
        } else {
            Some(out as f64 / input_bytes_for_ratio as f64)
        }
    });
    BenchResult {
        benchmark: benchmark.into(),
        tool: tool.into(),
        iterations: durations.len(),
        min_ms,
        median_ms,
        max_ms,
        bytes,
        throughput_mib_per_s: throughput,
        output_size,
        ratio,
    }
}

fn run_silent(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawn {:?}", cmd))?;
    if !status.success() {
        bail!("{:?} exited with {}", cmd, status);
    }
    Ok(())
}

fn bench_create(
    corpus: &Path,
    corpus_bytes: u64,
    iterations: usize,
    tools: &Tools,
    scratch: &Path,
) -> Result<Vec<BenchResult>> {
    let mut results = Vec::new();

    // tzip (deflate, default level)
    {
        let out = scratch.join("tzip.zip");
        let bin = tools.tzip.clone();
        let input = corpus.to_path_buf();
        let durations = time_cmd(
            || {
                let _ = fs::remove_file(&out);
                run_silent(
                    Command::new(&bin)
                        .args(["create"])
                        .arg(&out)
                        .arg(&input)
                        .arg("-q"),
                )
            },
            iterations,
        )?;
        let size = fs::metadata(&out).map(|m| m.len()).ok();
        results.push(summarize("create", "tzip (deflate)", &durations, corpus_bytes, size, corpus_bytes));
    }

    if let Some(zip_bin) = &tools.zip {
        let out = scratch.join("info-zip.zip");
        let bin = zip_bin.clone();
        let input = corpus.to_path_buf();
        let durations = time_cmd(
            || {
                let _ = fs::remove_file(&out);
                run_silent(
                    Command::new(&bin)
                        .args(["-q", "-r"])
                        .arg(&out)
                        .arg(".")
                        .current_dir(&input),
                )
            },
            iterations,
        )?;
        let size = fs::metadata(&out).map(|m| m.len()).ok();
        results.push(summarize("create", "zip (Info-ZIP)", &durations, corpus_bytes, size, corpus_bytes));
    }

    if let Some(sz) = &tools.seven_zip {
        let out = scratch.join("7z.zip");
        let bin = sz.clone();
        let input = corpus.to_path_buf();
        let durations = time_cmd(
            || {
                let _ = fs::remove_file(&out);
                run_silent(
                    Command::new(&bin)
                        .arg("a")
                        .arg("-tzip")
                        .arg("-bd")
                        .arg(&out)
                        .arg(&input),
                )
            },
            iterations,
        )?;
        let size = fs::metadata(&out).map(|m| m.len()).ok();
        results.push(summarize("create", "7z (zip)", &durations, corpus_bytes, size, corpus_bytes));
    }

    Ok(results)
}

fn bench_extract(
    iterations: usize,
    tools: &Tools,
    scratch: &Path,
) -> Result<Vec<BenchResult>> {
    let mut results = Vec::new();

    // Each tool extracts its own archive (produced by bench_create).
    // Skip rows whose archive isn't on disk (because the creator tool
    // wasn't available, or `--only extract` was used).
    let cases = [
        ("tzip (deflate)", scratch.join("tzip.zip"), tool_action::Tzip),
        ("zip (Info-ZIP)", scratch.join("info-zip.zip"), tool_action::Unzip),
        ("7z (zip)", scratch.join("7z.zip"), tool_action::SevenZip),
    ];

    for (label, archive, action) in cases {
        if !archive.exists() {
            continue;
        }
        let bytes = fs::metadata(&archive).ok().map(|m| m.len()).unwrap_or(0);
        let dest_base = scratch.join(format!(
            "x-{}",
            archive.file_stem().unwrap_or_default().to_string_lossy()
        ));
        let bin = match action {
            tool_action::Tzip => Some(tools.tzip.clone()),
            tool_action::Unzip => tools.unzip.as_ref().map(PathBuf::from),
            tool_action::SevenZip => tools.seven_zip.as_ref().map(PathBuf::from),
        };
        let Some(bin) = bin else { continue };

        let durations = time_cmd(
            || {
                let _ = fs::remove_dir_all(&dest_base);
                fs::create_dir_all(&dest_base)?;
                match action {
                    tool_action::Tzip => run_silent(
                        Command::new(&bin)
                            .args(["x"])
                            .arg(&archive)
                            .arg("--to")
                            .arg(&dest_base)
                            .arg("-q"),
                    ),
                    tool_action::Unzip => run_silent(
                        Command::new(&bin)
                            .args(["-q"])
                            .arg(&archive)
                            .arg("-d")
                            .arg(&dest_base),
                    ),
                    tool_action::SevenZip => run_silent(
                        Command::new(&bin)
                            .arg("x")
                            .arg("-bd")
                            .arg("-y")
                            .arg(format!("-o{}", dest_base.display()))
                            .arg(&archive),
                    ),
                }
            },
            iterations,
        )?;
        results.push(summarize("extract", label, &durations, bytes, None, bytes));
    }

    Ok(results)
}

fn bench_list(
    iterations: usize,
    tools: &Tools,
    scratch: &Path,
) -> Result<Vec<BenchResult>> {
    // Reuse tzip.zip as the canonical archive for listing (all three
    // list tools are pure-metadata so the input archive's origin
    // doesn't materially affect timing).
    let archive = scratch.join("tzip.zip");
    if !archive.exists() {
        return Ok(vec![]);
    }
    let bytes = fs::metadata(&archive).ok().map(|m| m.len()).unwrap_or(0);

    let mut results = Vec::new();

    let cases: [(String, Option<PathBuf>, &[&str]); 3] = [
        (
            "tzip ls".to_string(),
            Some(tools.tzip.clone()),
            &["ls"],
        ),
        (
            "unzip -l".to_string(),
            tools.unzip.as_ref().map(PathBuf::from),
            &["-l"],
        ),
        (
            "7z l".to_string(),
            tools.seven_zip.as_ref().map(PathBuf::from),
            &["l", "-ba"],
        ),
    ];

    for (label, bin, base_args) in cases {
        let Some(bin) = bin else { continue };
        let archive = archive.clone();
        let base_args_owned: Vec<String> = base_args.iter().map(|s| s.to_string()).collect();
        let durations = time_cmd(
            || {
                run_silent(
                    Command::new(&bin)
                        .args(&base_args_owned)
                        .arg(&archive),
                )
            },
            iterations,
        )?;
        results.push(summarize("list", &label, &durations, bytes, None, bytes));
    }

    Ok(results)
}

#[allow(non_snake_case)]
mod tool_action {
    #[derive(Copy, Clone)]
    pub enum TAction {
        Tzip,
        Unzip,
        SevenZip,
    }
    pub use TAction::*;
}

// ── Table formatting ───────────────────────────────────────────────────────

fn emit_table(results: &[BenchResult]) {
    use std::collections::BTreeMap;
    let mut by_bench: BTreeMap<&str, Vec<&BenchResult>> = BTreeMap::new();
    for r in results {
        by_bench.entry(r.benchmark.as_str()).or_default().push(r);
    }

    for (bench, rows) in by_bench {
        println!();
        println!("=== {} ===", bench);
        println!(
            "{:<20} {:>10} {:>10} {:>10} {:>14} {:>12} {:>7}",
            "tool", "median", "min", "max", "throughput", "size", "ratio"
        );
        for r in rows {
            let size = r
                .output_size
                .map(|s| format_bytes(s))
                .unwrap_or_default();
            let ratio = r
                .ratio
                .map(|r| format!("{:.1}%", r * 100.0))
                .unwrap_or_default();
            println!(
                "{:<20} {:>10} {:>10} {:>10} {:>14} {:>12} {:>7}",
                r.tool,
                format_ms(r.median_ms),
                format_ms(r.min_ms),
                format_ms(r.max_ms),
                format!("{:.1} MiB/s", r.throughput_mib_per_s),
                size,
                ratio,
            );
        }
    }
    println!();
}

fn format_ms(ms: u128) -> String {
    if ms >= 1000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        format!("{} ms", ms)
    }
}

fn format_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{} B", b as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_is_deterministic() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn synthetic_corpus_quick_hits_expected_size_range() {
        let td = tempfile::tempdir().unwrap();
        let bytes = generate_synthetic_corpus(td.path(), true).unwrap();
        // --quick targets ~5 MiB; allow a generous window.
        assert!(bytes > 1_000_000, "corpus too small: {}", bytes);
        assert!(bytes < 30_000_000, "corpus too big: {}", bytes);
    }

    #[test]
    fn format_bytes_sane_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2_048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn format_ms_switches_at_1s() {
        assert_eq!(format_ms(42), "42 ms");
        assert_eq!(format_ms(1_234), "1.23 s");
    }
}
