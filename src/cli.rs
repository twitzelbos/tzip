use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::{ArgMatches, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Tracks which auto-tunable flags came from the command line (vs took the
/// default). Auto-tune only overrides fields marked `false` here.
#[derive(Clone, Copy, Debug, Default)]
pub struct UserSetFlags {
    pub keep_cache: bool,
    pub read_jobs: bool,
    pub dispatch_io: bool,
    pub raw_block: bool,
    #[allow(dead_code)]
    pub io_uring: bool,
}

impl UserSetFlags {
    /// Call with the matches of the `create` or `u` subcommand.
    pub fn from_matches(m: &ArgMatches) -> Self {
        let known = |name: &str| m.try_contains_id(name).unwrap_or(false);
        let is_cli =
            |name: &str| known(name) && m.value_source(name) == Some(ValueSource::CommandLine);
        Self {
            keep_cache: is_cli("keep_cache"),
            read_jobs: is_cli("read_jobs"),
            dispatch_io: is_cli("dispatch_io"),
            raw_block: is_cli("raw_block"),
            io_uring: is_cli("io_uring"),
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

// ── Top-level CLI ──────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "tzip",
    version,
    about = "Parallel ZIP packer with hardware AES-256, external-drive-friendly I/O, and a cloud-ready reader stack."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Create a new archive from input paths.
    #[command(alias = "a")]
    Create(CreateArgs),

    /// Modify an existing ZIP: add / replace entries, delete entries via `--delete`.
    U(UpdateArgs),

    /// List entries in an archive.
    #[command(alias = "l")]
    Ls(LsArgs),

    /// Stream a single entry's decrypted + decompressed bytes to stdout.
    Cat(CatArgs),

    /// Extract one or more entries (or the whole archive).
    #[command(alias = "extract")]
    X(ExtractArgs),
}

// ── Create ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Output archive path (.zip, or .7z for solid LZMA2).
    pub archive: PathBuf,

    /// One or more input files or directories. With `--apfs-device`, these
    /// are interpreted as absolute in-volume paths; omit them to archive
    /// the whole volume.
    #[arg(required = false)]
    pub paths: Vec<PathBuf>,

    #[arg(short = 'm', long, value_enum, default_value_t = Method::Deflate)]
    pub method: Method,

    /// deflate 0-12 (libdeflater), bzip2 1-9, lzma/xz 0-9, zstd 1-22.
    #[arg(short = 'x', long, default_value_t = 6)]
    pub level: u32,

    #[arg(long)]
    pub solid: bool,

    #[arg(long)]
    pub tui: bool,

    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    #[arg(long, default_value = "2")]
    pub read_jobs: String,

    #[arg(long)]
    pub walk_jobs: Option<usize>,

    #[arg(long)]
    pub classic_walk: bool,

    #[arg(long)]
    pub dispatch_io: bool,

    #[arg(long)]
    pub usb_info: bool,

    #[cfg(any(
        all(feature = "raw-apfs", target_os = "macos"),
        target_os = "linux"
    ))]
    #[arg(
        long,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
    )]
    pub raw_block: bool,

    #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
    #[arg(long, value_name = "DEVICE")]
    pub apfs_device: Option<PathBuf>,

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    #[arg(long)]
    pub io_uring: bool,

    #[arg(long)]
    pub basename_only: bool,

    #[arg(long)]
    pub warn_contention: bool,

    #[arg(short = 'p', long)]
    pub password: Option<String>,

    #[arg(long, conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    #[arg(long)]
    pub sort: bool,

    #[arg(long)]
    pub exclude: Vec<String>,

    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,

    #[arg(long)]
    pub keep_cache: bool,

    #[arg(long)]
    pub streaming: bool,
}

// ── Update ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Target archive. Must exist (if missing, falls back to create).
    pub archive: PathBuf,

    /// Files or directories to add or replace in the archive.
    #[arg(required = false)]
    pub paths: Vec<PathBuf>,

    #[arg(short = 'm', long, value_enum, default_value_t = Method::Deflate)]
    pub method: Method,

    #[arg(short = 'x', long, default_value_t = 6)]
    pub level: u32,

    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    #[arg(long, default_value = "2")]
    pub read_jobs: String,

    #[arg(long)]
    pub walk_jobs: Option<usize>,

    /// Delete archive entries matching this glob. Repeatable.
    #[arg(long)]
    pub delete: Vec<String>,

    /// Force rebuild-to-temp + atomic rename (default: in-place append when safe).
    #[arg(long)]
    pub atomic: bool,

    #[arg(long)]
    pub basename_only: bool,

    #[arg(short = 'p', long)]
    pub password: Option<String>,

    #[arg(long, conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    #[arg(long)]
    pub sort: bool,

    #[arg(long)]
    pub exclude: Vec<String>,

    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,

    #[arg(long)]
    pub keep_cache: bool,

    #[arg(long)]
    pub tui: bool,
}

// ── Reader (ls / cat / x) ──────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Archive to list.
    pub archive: PathBuf,

    /// Password for AES-encrypted archives. Not needed for `ls` output
    /// itself; accepted for parity with `cat` / `x`.
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    #[arg(long, conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct CatArgs {
    pub archive: PathBuf,
    /// Archive-internal name of the entry to stream.
    pub name: String,

    #[arg(short = 'p', long)]
    pub password: Option<String>,

    #[arg(long, conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct ExtractArgs {
    pub archive: PathBuf,
    /// Specific entries to extract. Empty = extract the whole archive.
    #[arg(required = false)]
    pub names: Vec<String>,

    /// Extraction destination directory. Default: current directory.
    #[arg(long, value_name = "DIR")]
    pub to: Option<PathBuf>,

    #[arg(short = 'p', long)]
    pub password: Option<String>,

    #[arg(long, conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,
}

// ── Options structs consumed by the backends ──────────────────────────────

/// Shared options for both `create` and `u`. `update`/`delete`/`atomic` are
/// set by `UpdateArgs::into_options`; `CreateArgs::into_options` leaves them
/// at their defaults (update=false, delete empty, atomic=false).
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
    #[allow(dead_code)]
    pub streaming: bool,
    pub solid: bool,
    pub tui: bool,
    pub classic_walk: bool,
    pub dispatch_io: bool,
    pub usb_info: bool,
    pub warn_contention: bool,
    #[cfg(any(
        all(feature = "raw-apfs", target_os = "macos"),
        target_os = "linux"
    ))]
    pub raw_block: bool,
    #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
    pub apfs_device: Option<PathBuf>,
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    pub io_uring: bool,
    pub basename_only: bool,
    pub user_flags: UserSetFlags,
    pub update: bool,
    pub delete: Vec<String>,
    pub atomic: bool,
}

/// Which reader op is running and what it needs.
#[derive(Clone, Debug)]
pub enum ReaderOp {
    Ls,
    Cat { name: String },
    Extract { to: PathBuf, names: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct ReaderOptions {
    pub archive: PathBuf,
    pub password: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub op: ReaderOp,
}

// ── CreateArgs / UpdateArgs → Options ─────────────────────────────────────

impl CreateArgs {
    pub fn into_options(self, user_flags: UserSetFlags) -> Result<Options> {
        let cpu_jobs = self
            .jobs
            .unwrap_or_else(|| num_cpus::get_physical().max(1));

        let read_jobs = parse_read_jobs(&self.read_jobs, cpu_jobs)?;
        let walk_jobs = self.walk_jobs.unwrap_or_else(|| cpu_jobs.min(4).max(1));

        let solid = self.solid
            || self
                .archive
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.eq_ignore_ascii_case("7z"))
                .unwrap_or(false);

        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;

        let mut paths = self.paths;
        #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
        {
            if self.apfs_device.is_some() && paths.is_empty() {
                paths.push(PathBuf::from("/"));
            }
        }
        if paths.is_empty() {
            bail!(
                "need at least one input path \
                 (or use --apfs-device to archive a whole APFS volume)"
            );
        }

        Ok(Options {
            archive: self.archive,
            paths,
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
            warn_contention: self.warn_contention,
            #[cfg(any(
                all(feature = "raw-apfs", target_os = "macos"),
                target_os = "linux"
            ))]
            raw_block: self.raw_block,
            #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
            apfs_device: self.apfs_device,
            #[cfg(all(feature = "io-uring", target_os = "linux"))]
            io_uring: self.io_uring,
            basename_only: self.basename_only,
            user_flags,
            update: false,
            delete: Vec::new(),
            atomic: false,
        })
    }
}

impl UpdateArgs {
    pub fn into_options(self, user_flags: UserSetFlags) -> Result<Options> {
        let cpu_jobs = self
            .jobs
            .unwrap_or_else(|| num_cpus::get_physical().max(1));
        let read_jobs = parse_read_jobs(&self.read_jobs, cpu_jobs)?;
        let walk_jobs = self.walk_jobs.unwrap_or_else(|| cpu_jobs.min(4).max(1));

        let solid = self
            .archive
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.eq_ignore_ascii_case("7z"))
            .unwrap_or(false);
        if solid {
            bail!(".7z archives do not support `u` — rebuild the .7z from source");
        }

        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;

        if self.paths.is_empty() && self.delete.is_empty() {
            bail!("`tzip u` needs at least one input path or --delete GLOB");
        }

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
            streaming: false,
            solid: false,
            tui: self.tui,
            classic_walk: false,
            dispatch_io: false,
            usb_info: false,
            warn_contention: false,
            #[cfg(any(
                all(feature = "raw-apfs", target_os = "macos"),
                target_os = "linux"
            ))]
            raw_block: false,
            #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
            apfs_device: None,
            #[cfg(all(feature = "io-uring", target_os = "linux"))]
            io_uring: false,
            basename_only: self.basename_only,
            user_flags,
            update: true,
            delete: self.delete,
            atomic: self.atomic,
        })
    }
}

// ── Reader args → ReaderOptions ───────────────────────────────────────────

impl LsArgs {
    pub fn into_options(self) -> Result<ReaderOptions> {
        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;
        Ok(ReaderOptions {
            archive: self.archive,
            password,
            quiet: self.quiet,
            verbose: self.verbose,
            op: ReaderOp::Ls,
        })
    }
}

impl CatArgs {
    pub fn into_options(self) -> Result<ReaderOptions> {
        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;
        Ok(ReaderOptions {
            archive: self.archive,
            password,
            quiet: self.quiet,
            verbose: self.verbose,
            op: ReaderOp::Cat { name: self.name },
        })
    }
}

impl ExtractArgs {
    pub fn into_options(self) -> Result<ReaderOptions> {
        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;
        let to = self.to.unwrap_or_else(|| PathBuf::from("."));
        Ok(ReaderOptions {
            archive: self.archive,
            password,
            quiet: self.quiet,
            verbose: self.verbose,
            op: ReaderOp::Extract { to, names: self.names },
        })
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

fn parse_read_jobs(raw: &str, cpu_jobs: usize) -> Result<usize> {
    if raw.eq_ignore_ascii_case("auto") {
        return Ok(cpu_jobs.min(8).max(1));
    }
    let n: usize = raw
        .parse()
        .with_context(|| format!("invalid --read-jobs {:?}", raw))?;
    if n == 0 {
        bail!("--read-jobs must be at least 1");
    }
    Ok(n)
}

fn resolve_password(password: Option<&str>, password_file: Option<&std::path::Path>) -> Result<Option<String>> {
    if let Some(pf) = password_file {
        let text = std::fs::read_to_string(pf)
            .with_context(|| format!("read password file {}", pf.display()))?;
        let first = text.lines().next().unwrap_or("").to_string();
        if first.is_empty() {
            bail!("password file is empty");
        }
        return Ok(Some(first));
    }
    match password {
        None => Ok(None),
        Some(s) if s == "-" => {
            let pw = rpassword::prompt_password("Password: ").context("read password")?;
            if pw.is_empty() {
                bail!("empty password");
            }
            Ok(Some(pw))
        }
        Some(s) => Ok(Some(s.to_string())),
    }
}
