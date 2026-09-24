use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::{ArgMatches, Parser, ValueEnum};
use std::path::PathBuf;

/// Tracks which auto-tunable flags came from the command line (vs took the
/// default). Auto-tune only overrides fields marked `false` here.
#[derive(Clone, Copy, Debug, Default)]
pub struct UserSetFlags {
    pub keep_cache: bool,
    pub read_jobs: bool,
    pub dispatch_io: bool,
}

impl UserSetFlags {
    pub fn from_matches(m: &ArgMatches) -> Self {
        let is_cli = |name: &str| m.value_source(name) == Some(ValueSource::CommandLine);
        Self {
            keep_cache: is_cli("keep_cache"),
            read_jobs: is_cli("read_jobs"),
            dispatch_io: is_cli("dispatch_io"),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Method {
    Store,
    Deflate,
    Bzip2,
    Lzma,
    Xz,
    Zstd,
}

#[derive(Parser, Debug)]
#[command(
    name = "tzip",
    version,
    about = "The fastest ZIP packer this side of the M1 — parallel, hardware-AES-256, external-drive-friendly",
    long_about = None,
)]
pub struct Args {
    /// Output archive path (.zip)
    pub archive: PathBuf,

    /// One or more input files or directories
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,

    /// Compression method
    #[arg(short = 'm', long, value_enum, default_value_t = Method::Deflate)]
    pub method: Method,

    /// Compression level. Range varies by method:
    /// deflate 0-12 (libdeflater), bzip2 1-9, lzma/xz 0-9, zstd 1-22.
    /// Auto-clamped to each method's supported range.
    #[arg(short = 'x', long, default_value_t = 6)]
    pub level: u32,

    /// Emit a `.7z` solid archive (LZMA2 solid block, header encryption when password given).
    /// Also enabled automatically when the archive path ends in `.7z`.
    #[arg(long)]
    pub solid: bool,

    /// Enable ratatui dashboard when stdout is a TTY.
    #[arg(long)]
    pub tui: bool,

    /// Number of CPU workers for compression+encryption (default: physical cores)
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Number of reader threads that touch the filesystem.
    /// Keep this low (1 or 2) for external / spinning drives — high values
    /// stampede the disk. Pass `auto` to scale to `min(cpu_jobs, 8)` for
    /// internal NVMe. Default is `2` (safe on external drives).
    #[arg(long, default_value = "2")]
    pub read_jobs: String,

    /// Number of directory-scan threads. `readdir` doesn't seek-storm the
    /// way file *content* reads do, so this can be higher than `--read-jobs`.
    /// Default is `4` (or `cpu_jobs` if smaller).
    #[arg(long)]
    pub walk_jobs: Option<usize>,

    /// Disable the macOS-only bulk walker (`getattrlistbulk`) and use the
    /// portable jwalk-based walker. Only useful for debugging.
    #[arg(long)]
    pub classic_walk: bool,

    /// Use Grand Central Dispatch's `dispatch_io_read` for file body reads
    /// (macOS only, small files only — large files still mmap). Experimental
    /// alternative to the default POSIX read path; may help on USB-MSC.
    #[arg(long)]
    pub dispatch_io: bool,

    /// Print a USB/drive diagnostic for any inputs or output on a USB bus
    /// before archiving. macOS only; a no-op elsewhere. Shows negotiated link
    /// speed, filesystem, media name, and tuning advisories.
    #[arg(long)]
    pub usb_info: bool,

    /// Encryption password. Use `-` to read from stdin (tty prompt if interactive).
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    /// Read password from a file (first line)
    #[arg(long, conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    /// Sort entries by path — produces reproducible byte-identical archives
    #[arg(long)]
    pub sort: bool,

    /// Exclude paths matching this glob (repeatable)
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Silence progress bar
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Per-file logging to stderr
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Skip macOS F_NOCACHE — keep read blocks in the page cache
    /// (default: bypass page cache so the archive pass doesn't evict your working set)
    #[arg(long)]
    pub keep_cache: bool,

    /// Write a data descriptor after each entry instead of seeking back to patch the LFH.
    /// Enabled automatically when the output is not a regular seekable file.
    #[arg(long)]
    pub streaming: bool,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub archive: PathBuf,
    pub paths: Vec<PathBuf>,
    pub method: Method,
    pub level: u32,
    pub cpu_jobs: usize,
    pub read_jobs: usize,
    pub walk_jobs: usize,
    pub password: Option<String>,
    pub sort: bool,
    pub exclude: Vec<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub keep_cache: bool,
    // Reserved for a future streaming (non-seekable) writer path.
    #[allow(dead_code)]
    pub streaming: bool,
    pub solid: bool,
    pub tui: bool,
    pub classic_walk: bool,
    pub dispatch_io: bool,
    pub usb_info: bool,
    pub user_flags: UserSetFlags,
}

impl Args {
    pub fn into_options(self, user_flags: UserSetFlags) -> Result<Options> {
        let cpu_jobs = self
            .jobs
            .unwrap_or_else(|| num_cpus::get_physical().max(1));

        let read_jobs = if self.read_jobs.eq_ignore_ascii_case("auto") {
            cpu_jobs.min(8).max(1)
        } else {
            let n: usize = self
                .read_jobs
                .parse()
                .with_context(|| format!("invalid --read-jobs {:?}", self.read_jobs))?;
            if n == 0 {
                bail!("--read-jobs must be at least 1");
            }
            n
        };

        let password = resolve_password(&self)?;

        let walk_jobs = self.walk_jobs.unwrap_or_else(|| cpu_jobs.min(4).max(1));

        let solid = self.solid
            || self
                .archive
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.eq_ignore_ascii_case("7z"))
                .unwrap_or(false);

        Ok(Options {
            archive: self.archive,
            paths: self.paths,
            method: self.method,
            level: self.level,
            cpu_jobs,
            read_jobs,
            walk_jobs,
            password,
            sort: self.sort,
            exclude: self.exclude,
            quiet: self.quiet,
            verbose: self.verbose,
            keep_cache: self.keep_cache,
            streaming: self.streaming,
            solid,
            tui: self.tui,
            classic_walk: self.classic_walk,
            dispatch_io: self.dispatch_io,
            usb_info: self.usb_info,
            user_flags,
        })
    }
}

fn resolve_password(args: &Args) -> Result<Option<String>> {
    if let Some(pf) = &args.password_file {
        let text = std::fs::read_to_string(pf)
            .with_context(|| format!("read password file {}", pf.display()))?;
        let first = text.lines().next().unwrap_or("").to_string();
        if first.is_empty() {
            bail!("password file is empty");
        }
        return Ok(Some(first));
    }
    match &args.password {
        None => Ok(None),
        Some(s) if s == "-" => {
            let pw = rpassword::prompt_password("Password: ").context("read password")?;
            if pw.is_empty() {
                bail!("empty password");
            }
            Ok(Some(pw))
        }
        Some(s) => Ok(Some(s.clone())),
    }
}
