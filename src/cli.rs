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
    /// PPMd variant Ir1 (ZIP method 98). Context-modelling entropy
    /// coder — great ratio on text, CPU-bound on compression, needs
    /// a few MB to a few hundred MB of model arena per entry (scales
    /// with `--level`). Recipient needs a PPMd-aware unzipper
    /// (7-Zip, WinZip, Info-ZIP builds with PPMd).
    Ppmd,
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

    /// Search for a regex pattern across many archives in parallel.
    /// Works against local paths, http(s):// URLs, or s3://bucket/key
    /// URLs. S3 bucket-globbing (`s3://bucket/prefix/*.zip`) is
    /// expanded via ListObjectsV2. See `tzip grep --help` for all
    /// fan-out, filter, and checkpoint flags.
    #[command(alias = "g")]
    Grep(GrepArgs),

    /// Mount an archive as a read-only filesystem. Linux FUSE3 ships
    /// in this release; macOS FSKit support is a planned follow-up PR.
    Mount(MountArgs),

    /// List entries across one or more archives matching CDR-side
    /// filters (name glob, size, mtime). Body-free — never fetches
    /// entry contents, so it's essentially free on remote archives.
    /// Same `-P` fan-out + S3 bucket globbing as `tzip grep`.
    #[command(alias = "f")]
    Find(FindArgs),

    /// Benchmark tzip against system `7z` / `zip` / `unzip` on a
    /// reproducible corpus. Measures create speed, extract speed, list
    /// speed, output size, and compression ratio. External tools are
    /// detected on PATH and skipped when missing.
    Bench(BenchArgs),

    /// Body-free CDR comparison of two archives. Reports entries
    /// added in b, removed from a, and changed (same name, different
    /// crc32 or size). Works on local paths, http(s):// URLs, and
    /// s3://bucket/key URLs. Exit status: 0 = identical, 1 = differ,
    /// 2 = error.
    #[command(alias = "d")]
    Diff(DiffArgs),

    /// Hash entries in an archive. `--algo crc32` reads the CRC
    /// straight out of the central directory — zero body I/O, free
    /// for HTTP/S3. `--algo md5` or `--algo sha256` streams each
    /// entry's body through a hasher (decompression + decrypt done
    /// in parallel workers, same model as `tzip x`).
    #[command(alias = "h")]
    Hash(HashArgs),

    /// Aggregate over a per-field facet index built by
    /// `tzip create --facet '.field'`. Emits a sorted count-by-value
    /// table in milliseconds, no body decode — the archive is the
    /// database. `tzip aggregate --fields archive.zip` lists the
    /// available facets.
    #[command(alias = "agg")]
    Aggregate(AggregateArgs),
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

    /// Compute and stash MD5 of every entry's plaintext in a tzip-native
    /// extra field, so later `tzip hash --algo md5` reads can skip the
    /// body-decode round-trip. Hashing fuses with the CRC pass so the
    /// extra cost is roughly a single memcpy over each uncompressed
    /// body. Other tools ignore the field per APPNOTE.
    #[arg(long)]
    pub write_md5: bool,

    /// Same as `--write-md5` but for SHA-256. Can be combined with
    /// `--write-md5`; both digests land in the same entry.
    #[arg(long)]
    pub write_sha256: bool,

    /// Write a minimal-perfect-hash index over entry names as a
    /// `.tzip/names.v1` entry at the end of the archive. Lets
    /// `tzip cat NAME` / `tzip x NAME` / entry-lookup paths run in
    /// O(1) time regardless of archive size. ~3 bits per entry,
    /// built in milliseconds per million names. Other tools see it
    /// as a normal (small) ZIP entry and ignore it.
    #[arg(long)]
    pub index_names: bool,

    /// Build a hashed inverted term index over text entries whose
    /// name matches this glob (repeatable). Stored as `.tzip/terms.v1`
    /// inside the archive. Later `tzip grep LITERAL archive.zip`
    /// consults the index to find the set of entries that could
    /// contain each literal token, then runs its regex scan only
    /// against those — sub-second grep across archives where the
    /// term is rare. Regex queries that don't contain any literal
    /// tokens fall back to full linear scan.
    #[arg(long, value_name = "GLOB")]
    pub index_terms: Vec<String>,

    /// Build a per-field facet index over JSON entries whose name
    /// matches `--index-terms` (repeatable). Spec is a jq-style
    /// dot-path, e.g. `.status_code` or `.event.kind`. Stored as
    /// `.tzip/facets.v1/<leaf>` inside the archive. Later
    /// `tzip aggregate --by <leaf> --count archive.zip` returns a
    /// sorted count-by-value table in milliseconds, with no body
    /// decode. String / number / bool values are counted as-is;
    /// missing or non-scalar fields fall into a `__missing__`
    /// bucket so totals always equal entry counts.
    #[arg(long, value_name = "PATH")]
    pub facet: Vec<String>,
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

    /// Compute + stash plaintext MD5 for every added or replaced entry
    /// in a tzip-native extra field. If the archive already carries MD5
    /// hashes on its existing entries, this is auto-enabled regardless
    /// so added/replaced entries match the archive's existing format.
    #[arg(long)]
    pub write_md5: bool,

    /// Same as `--write-md5` for SHA-256. Auto-enabled if the archive
    /// already carries SHA-256 hashes on existing entries.
    #[arg(long)]
    pub write_sha256: bool,
}

// ── Reader (ls / cat / x) ──────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Archive to list. Accepts a local file path, an `http(s)://` URL, or
    /// an `s3://bucket/key` URL (requires AWS credentials in the usual env
    /// vars or `~/.aws/credentials`).
    pub archive: String,

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
    /// Archive. Accepts a local file path, an `http(s)://` URL, or an
    /// `s3://bucket/key` URL.
    pub archive: String,
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
pub struct FindArgs {
    /// One or more archives. Local paths, `http(s)://` URLs, or
    /// `s3://bucket/key.zip` URLs. S3 URLs may contain `*` / `?`
    /// wildcards in the key portion (expanded via ListObjectsV2).
    #[arg(required = true)]
    pub archives: Vec<String>,

    #[arg(short = 'P', long, default_value_t = 32)]
    pub parallel: usize,

    /// Only list entries matching this glob (repeatable).
    #[arg(long)]
    pub include: Vec<String>,

    /// Skip entries matching this glob (repeatable).
    #[arg(long)]
    pub exclude: Vec<String>,

    #[arg(long)]
    pub min_size: Option<u64>,
    #[arg(long)]
    pub max_size: Option<u64>,

    #[arg(long, value_name = "YYYY-MM-DD")]
    pub since: Option<String>,
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub until: Option<String>,

    /// Emit one entry name per line, no other fields.
    #[arg(long)]
    pub names_only: bool,

    /// Emit entries as JSON lines instead of the default tabular format.
    #[arg(long, conflicts_with = "names_only")]
    pub json: bool,

    #[arg(short = 'q', long)]
    pub quiet: bool,
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Baseline archive. Local path, `http(s)://` URL, or
    /// `s3://bucket/key.zip` URL.
    pub a: String,
    /// Comparison archive. Same source types as `a`.
    pub b: String,

    /// Only consider entries matching this glob (repeatable).
    #[arg(long)]
    pub include: Vec<String>,
    /// Skip entries matching this glob (repeatable).
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Emit one `status entry` per line, nothing else.
    #[arg(long)]
    pub names_only: bool,
    /// Emit entries as JSON lines instead of the default tabular format.
    #[arg(long, conflicts_with = "names_only")]
    pub json: bool,
    /// Also report entries whose only difference is mtime (quiet by
    /// default — mtime drift is noisy on repackaged archives).
    #[arg(long)]
    pub mtime: bool,

    #[arg(short = 'q', long)]
    pub quiet: bool,
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Directory to use as the input corpus. If omitted, a
    /// deterministic synthetic corpus is generated in a tempdir
    /// (mix of text + random binary, ~50 MB default or ~5 MB with
    /// `--quick`).
    #[arg(long, value_name = "DIR")]
    pub corpus: Option<PathBuf>,

    /// Iterations per benchmark. Median is reported alongside min/max.
    #[arg(short = 'n', long, default_value_t = 3)]
    pub iterations: usize,

    /// Shorter corpus + 1 iteration — suitable for a CI-time sanity
    /// check. Implies `-n 1`.
    #[arg(long)]
    pub quick: bool,

    /// Emit results as JSON lines instead of a human-readable table.
    #[arg(long)]
    pub json: bool,

    /// Restrict to a comma-separated subset: `create`, `extract`, `list`.
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,

    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct GrepArgs {
    /// Regex pattern. Syntax is the `regex` crate's — same engine
    /// ripgrep uses. For advanced features (PCRE2, context lines,
    /// colored output), pipe through `--exec rg` instead.
    pub pattern: String,

    /// One or more archives. Local paths, `http(s)://` URLs, or
    /// `s3://bucket/key.zip` URLs. S3 URLs may contain `*` / `?`
    /// wildcards in the key portion (expanded via ListObjectsV2).
    /// Local file globs are handled by your shell.
    #[arg(required = true)]
    pub archives: Vec<String>,

    /// Concurrent archives in flight.
    #[arg(short = 'P', long, default_value_t = 32)]
    pub parallel: usize,

    /// Concurrent entries scanned within one archive.
    #[arg(long, default_value_t = 8)]
    pub inner_jobs: usize,

    /// Cap on the shared HTTP(S) / S3 connection pool. Default =
    /// parallel × inner_jobs, capped at 256.
    #[arg(long)]
    pub connections: Option<usize>,

    /// Only scan entries matching this glob (repeatable). Applied at
    /// the CDR so skipped entries never trigger a body fetch.
    #[arg(long)]
    pub include: Vec<String>,

    /// Skip entries matching this glob (repeatable). Also applied at
    /// the CDR.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Skip entries smaller than this (bytes).
    #[arg(long)]
    pub min_size: Option<u64>,

    /// Skip entries larger than this (bytes).
    #[arg(long)]
    pub max_size: Option<u64>,

    /// Skip entries modified before this ISO date (YYYY-MM-DD).
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub since: Option<String>,

    /// Skip entries modified after this ISO date (YYYY-MM-DD).
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub until: Option<String>,

    /// Case-insensitive match.
    #[arg(short = 'i', long)]
    pub ignore_case: bool,

    /// Only emit `archive:entry` for entries with any match.
    #[arg(long)]
    pub files_only: bool,

    /// Emit matches as JSON lines instead of grep-style output.
    #[arg(long)]
    pub json: bool,

    /// Instead of matching in-process, write each candidate entry to
    /// a tempfile and run CMD with `{}` replaced by the path.
    /// Example: `--exec 'rg -C 3 --json {}'`.
    #[arg(long, value_name = "CMD")]
    pub exec: Option<String>,

    /// Append per-entry completion records to this JSONL file.
    #[arg(long, value_name = "FILE")]
    pub checkpoint: Option<PathBuf>,

    /// Resume a previous run: skip (archive, entry) pairs already in
    /// the checkpoint. Refuses to resume if the invocation's
    /// pattern or archive set differs from the checkpoint's header.
    #[arg(long, requires = "checkpoint")]
    pub resume: bool,

    /// Password for AES-encrypted entries.
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
pub struct MountArgs {
    /// Archive. Accepts a local path, `http(s)://` URL, or
    /// `s3://bucket/key` URL. Not required when `--install` or
    /// `--uninstall` is used.
    #[arg(required_unless_present_any = ["install", "uninstall"])]
    pub archive: Option<String>,
    /// Mount point (must exist, be empty, and be writable to the
    /// current user — standard FUSE requirements). Not required when
    /// `--install` or `--uninstall` is used.
    #[arg(required_unless_present_any = ["install", "uninstall"])]
    pub mountpoint: Option<PathBuf>,

    /// Memory budget for the per-entry decompressed-body LRU cache (MiB).
    #[arg(long, default_value_t = 256)]
    pub cache_mb: usize,

    /// Allow other users to read the mount. Adds `allow_other` to the
    /// FUSE options (Linux) / mount option (macOS FSKit); on Linux
    /// requires `user_allow_other` in /etc/fuse.conf.
    #[arg(long)]
    pub allow_other: bool,

    /// Open the mount read-write. Edits (create, write, unlink, mkdir,
    /// rmdir, truncate) are journaled in memory and committed to the
    /// archive atomically on unmount via `tzip u`. Linux FUSE3 only in
    /// this release — a crash before unmount loses the pending
    /// changes but never corrupts the archive. Local archives only;
    /// HTTP/S3 mounts stay read-only.
    #[arg(long)]
    pub write: bool,

    /// macOS + `--features fskit` only: register the tzip FSKit system
    /// extension. One-time per install; user has to approve in System
    /// Settings. No-op on Linux.
    #[arg(long, conflicts_with = "uninstall")]
    pub install: bool,

    /// macOS + `--features fskit` only: deregister the tzip FSKit
    /// system extension. No-op on Linux.
    #[arg(long)]
    pub uninstall: bool,

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
    /// Archive. Accepts a local file path, an `http(s)://` URL, or an
    /// `s3://bucket/key` URL.
    pub archive: String,
    /// Specific entries to extract. Empty = extract the whole archive.
    #[arg(required = false)]
    pub names: Vec<String>,

    /// Extraction destination directory. Default: current directory.
    #[arg(long, value_name = "DIR")]
    pub to: Option<PathBuf>,

    /// Concurrent entries in flight. Each worker holds its own backing
    /// source (its own HTTP/S3 connection or its own local fd) so Range
    /// GETs and decompression overlap.
    #[arg(short = 'P', long, default_value_t = 8)]
    pub parallel: usize,

    /// Cap on the shared HTTP(S) / S3 connection pool. Default = `parallel`.
    /// Local-path archives ignore this.
    #[arg(long)]
    pub connections: Option<usize>,

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
    /// When set, encode_one computes MD5 of the plaintext alongside
    /// CRC-32 and the writer stashes it in a tzip-native extra field.
    pub write_md5: bool,
    /// Same as `write_md5` for SHA-256.
    pub write_sha256: bool,
    /// When set, pipeline writes a `.tzip/names.v1` entry at the end
    /// of the archive carrying an MPH over all entry names.
    pub index_names: bool,
    /// Globs of entries to tokenize + index with a hashed inverted
    /// term index in `.tzip/terms.v1`. Empty = skip.
    pub index_terms: Vec<String>,
    /// jq-style field paths to accumulate facet counts for. Each
    /// becomes a `.tzip/facets.v1/<leaf>` entry. Only JSON entries
    /// matching `index_terms` glob contribute.
    pub facet: Vec<String>,
}

/// Which reader op is running and what it needs.
#[derive(Clone, Debug)]
pub enum ReaderOp {
    Ls,
    Cat { name: String },
    Extract {
        to: PathBuf,
        names: Vec<String>,
        parallel: usize,
        connections: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub struct GrepOptions {
    pub pattern: String,
    pub archives: Vec<String>,
    pub parallel: usize,
    pub inner_jobs: usize,
    pub connections: Option<usize>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// DOS-converted (date, time) values parsed from `--since` / `--until`.
    /// Unix timestamps (seconds) are the user-facing semantic; we store
    /// them for the per-entry mtime filter.
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub ignore_case: bool,
    pub files_only: bool,
    pub json: bool,
    pub exec: Option<String>,
    pub checkpoint: Option<PathBuf>,
    pub resume: bool,
    pub password: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub struct FindOptions {
    pub archives: Vec<String>,
    pub parallel: usize,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub names_only: bool,
    pub json: bool,
    pub quiet: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub struct DiffOptions {
    pub a: String,
    pub b: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub names_only: bool,
    pub json: bool,
    pub mtime: bool,
    pub quiet: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub struct BenchOptions {
    pub corpus: Option<PathBuf>,
    pub iterations: usize,
    pub quick: bool,
    pub json: bool,
    pub only: Vec<String>,
    pub quiet: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub struct ReaderOptions {
    /// Local path, `http(s)://` URL, or `s3://bucket/key` URL.
    pub archive: String,
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
            write_md5: self.write_md5,
            write_sha256: self.write_sha256,
            index_names: self.index_names,
            index_terms: self.index_terms,
            facet: self.facet,
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
            // Explicit `--write-md5` / `--write-sha256` on `tzip u`.
            // `run_update` may additionally set these to `true` after
            // peeking the archive, if the existing entries already
            // carry digests — so added/replaced entries match the
            // archive's existing format.
            write_md5: self.write_md5,
            write_sha256: self.write_sha256,
            // `tzip u` on an archive that already has a `.tzip/names.v1`
            // index invalidates it (new entries' indices shift). Phase 4
            // of the roadmap handles segmented index updates; for now
            // we skip index emission on update.
            index_names: false,
            index_terms: Vec::new(),
            facet: Vec::new(),
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

impl DiffArgs {
    pub fn into_options(self) -> Result<DiffOptions> {
        Ok(DiffOptions {
            a: self.a,
            b: self.b,
            include: self.include,
            exclude: self.exclude,
            names_only: self.names_only,
            json: self.json,
            mtime: self.mtime,
            quiet: self.quiet,
            verbose: self.verbose,
        })
    }
}

impl BenchArgs {
    pub fn into_options(self) -> Result<BenchOptions> {
        let iterations = if self.quick { 1 } else { self.iterations.max(1) };
        Ok(BenchOptions {
            corpus: self.corpus,
            iterations,
            quick: self.quick,
            json: self.json,
            only: self.only,
            quiet: self.quiet,
            verbose: self.verbose,
        })
    }
}

impl FindArgs {
    pub fn into_options(self) -> Result<FindOptions> {
        let since = parse_iso_date(self.since.as_deref(), "--since")?;
        let until = parse_iso_date(self.until.as_deref(), "--until")?;
        Ok(FindOptions {
            archives: self.archives,
            parallel: self.parallel.max(1),
            include: self.include,
            exclude: self.exclude,
            min_size: self.min_size,
            max_size: self.max_size,
            since,
            until,
            names_only: self.names_only,
            json: self.json,
            quiet: self.quiet,
            verbose: self.verbose,
        })
    }
}

impl GrepArgs {
    pub fn into_options(self) -> Result<GrepOptions> {
        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;
        let since = parse_iso_date(self.since.as_deref(), "--since")?;
        let until = parse_iso_date(self.until.as_deref(), "--until")?;
        Ok(GrepOptions {
            pattern: self.pattern,
            archives: self.archives,
            parallel: self.parallel.max(1),
            inner_jobs: self.inner_jobs.max(1),
            connections: self.connections,
            include: self.include,
            exclude: self.exclude,
            min_size: self.min_size,
            max_size: self.max_size,
            since,
            until,
            ignore_case: self.ignore_case,
            files_only: self.files_only,
            json: self.json,
            exec: self.exec,
            checkpoint: self.checkpoint,
            resume: self.resume,
            password,
            quiet: self.quiet,
            verbose: self.verbose,
        })
    }
}

/// Parse `YYYY-MM-DD` to a Unix timestamp at UTC midnight.
fn parse_iso_date(raw: Option<&str>, flag: &str) -> Result<Option<i64>> {
    let Some(s) = raw else { return Ok(None) };
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        bail!("{} expects YYYY-MM-DD, got {:?}", flag, s);
    }
    let y: i64 = parts[0].parse().with_context(|| format!("{}: bad year", flag))?;
    let m: u32 = parts[1].parse().with_context(|| format!("{}: bad month", flag))?;
    let d: u32 = parts[2].parse().with_context(|| format!("{}: bad day", flag))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        bail!("{}: out-of-range date {:?}", flag, s);
    }
    Ok(Some(civil_to_unix(y, m, d)))
}

use crate::dos_time::civil_to_unix;

impl MountArgs {
    pub fn into_action(self) -> Result<crate::mount::MountAction> {
        #[cfg(all(target_os = "macos", feature = "fskit"))]
        {
            if self.install {
                return Ok(crate::mount::MountAction::Install);
            }
            if self.uninstall {
                return Ok(crate::mount::MountAction::Uninstall);
            }
        }
        #[cfg(not(all(target_os = "macos", feature = "fskit")))]
        {
            if self.install || self.uninstall {
                bail!(
                    "--install / --uninstall are macOS-only and require the \
                     `fskit` cargo feature (build with `cargo build --features fskit`)"
                );
            }
        }
        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;
        let archive_spec = self
            .archive
            .ok_or_else(|| anyhow::anyhow!("archive is required"))?;
        let mountpoint = self
            .mountpoint
            .ok_or_else(|| anyhow::anyhow!("mountpoint is required"))?;
        Ok(crate::mount::MountAction::Mount(crate::mount::MountOptions {
            archive_spec,
            mountpoint,
            cache_mb: self.cache_mb,
            allow_other: self.allow_other,
            write: self.write,
            password,
            quiet: self.quiet,
            verbose: self.verbose,
        }))
    }
}

impl ExtractArgs {
    pub fn into_options(self) -> Result<ReaderOptions> {
        let password = resolve_password(self.password.as_deref(), self.password_file.as_deref())?;
        let to = self.to.unwrap_or_else(|| PathBuf::from("."));
        let parallel = self.parallel.max(1);
        Ok(ReaderOptions {
            archive: self.archive,
            password,
            quiet: self.quiet,
            verbose: self.verbose,
            op: ReaderOp::Extract {
                to,
                names: self.names,
                parallel,
                connections: self.connections,
            },
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

// ── Hash ──────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum HashAlgo {
    /// CRC-32 read from the central directory. Zero body I/O; free
    /// for HTTP / S3. Weak as a cryptographic hash (32 bits) but
    /// exact for change detection.
    Crc32,
    /// MD5 of the uncompressed body. Streams every entry through
    /// a decompressor + hasher in parallel workers. 128 bits.
    Md5,
    /// SHA-256 of the uncompressed body. Same streaming model as
    /// `md5`. 256 bits — the right default for anything security-
    /// adjacent.
    Sha256,
}

impl HashAlgo {
    pub fn needs_body(self) -> bool {
        !matches!(self, HashAlgo::Crc32)
    }
    pub fn label(self) -> &'static str {
        match self {
            HashAlgo::Crc32 => "crc32",
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha256 => "sha256",
        }
    }
}

#[derive(Args, Debug)]
pub struct HashArgs {
    /// Archive. Local path, `http(s)://` URL, or `s3://bucket/key.zip` URL.
    pub archive: String,

    /// Specific entry names to hash. Empty = all entries that pass
    /// `--include` / `--exclude`.
    #[arg(required = false)]
    pub names: Vec<String>,

    /// Hash algorithm.
    #[arg(long, value_enum, default_value_t = HashAlgo::Sha256)]
    pub algo: HashAlgo,

    /// Only hash entries matching this glob (repeatable).
    #[arg(long)]
    pub include: Vec<String>,

    /// Skip entries matching this glob (repeatable).
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Parallel decompress-hash workers (ignored by `--algo crc32`,
    /// which never touches entry bodies).
    #[arg(short = 'P', long, default_value_t = 8)]
    pub parallel: usize,

    /// Password for AES / ZipCrypto entries. `-` reads from a prompt.
    #[arg(short = 'p', long)]
    pub password: Option<String>,
    #[arg(long)]
    pub password_file: Option<PathBuf>,

    /// Emit as JSON lines (`{"name": "...", "algo": "sha256", "hash": "..."}`)
    /// instead of the default `<hex> <name>` tabular output.
    #[arg(long)]
    pub json: bool,

    #[arg(short = 'q', long)]
    pub quiet: bool,
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

// ── Aggregate ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct AggregateArgs {
    /// Archive. Local path, `http(s)://` URL, or `s3://bucket/key.zip` URL.
    pub archive: String,

    /// Field (leaf name) to aggregate by. Matches the leaf of the
    /// path used at create time (`--facet '.event.kind'` → use
    /// `--by kind`). Required unless `--fields` is set.
    #[arg(long, value_name = "FIELD")]
    pub by: Option<String>,

    /// List the facet fields available in this archive, one per
    /// line, and exit. Mutually exclusive with `--by`.
    #[arg(long, conflicts_with = "by")]
    pub fields: bool,

    /// Limit output to the top N values by count. 0 = all.
    #[arg(long, default_value_t = 0usize)]
    pub top: usize,

    /// Emit as JSON (one object per line: `{"value": ..., "count": N}`)
    /// instead of the default tabular layout.
    #[arg(long)]
    pub json: bool,

    #[arg(short = 'q', long)]
    pub quiet: bool,
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub struct AggregateOptions {
    pub archive: String,
    pub by: Option<String>,
    pub fields: bool,
    pub top: usize,
    pub json: bool,
    pub quiet: bool,
    pub verbose: bool,
}

impl AggregateArgs {
    pub fn into_options(self) -> Result<AggregateOptions> {
        if self.by.is_none() && !self.fields {
            bail!("tzip aggregate: pass --by FIELD or --fields");
        }
        Ok(AggregateOptions {
            archive: self.archive,
            by: self.by,
            fields: self.fields,
            top: self.top,
            json: self.json,
            quiet: self.quiet,
            verbose: self.verbose,
        })
    }
}
