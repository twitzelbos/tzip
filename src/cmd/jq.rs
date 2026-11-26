//! `tzip jq FILTER ARCHIVE...` — streamed JSON filter via `jaq`.
//!
//! For every entry in every passed archive (or `s3://bucket/*.zip`
//! glob — reuses Phase 7 shard expansion) whose name matches
//! `--include` (default `*.json`), decompress the body, parse it as
//! a single JSON value, apply the compiled filter, and emit each
//! output value on its own line. `-r/--raw-output` matches jq's
//! semantics: raw strings print without quotes, non-strings
//! serialise as JSON.
//!
//! One archive shard at a time; no rayon fan-out inside — `tzip
//! grep` already demonstrates the concurrency shape for remote
//! fan-out and the next iteration can borrow its outer pool when a
//! user actually bottlenecks on jq latency. Filter is compiled
//! once up front, then re-used across every entry.
//!
//! ## Compared to `tzip query`
//!
//! - `tzip query --sql ...` needs a columnar sidecar (schema
//!   fixed at create time) and a system `duckdb`; it's optimised
//!   for aggregates and joins over structured fields.
//! - `tzip jq FILTER` reads the raw entry bodies and runs an
//!   arbitrary jq program; no sidecar needed, filter is any valid
//!   jq expression.
//!
//! Use `query` when you want SQL ergonomics against pre-declared
//! columns; use `jq` when you want to pick at raw JSON with the
//! same flexibility you'd get piping `tzip cat ... | jq`.

use anyhow::{bail, Context, Result};
use jaq_core::load::{Arena, File as JaqFile, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::{read, Val};
use std::io::{BufWriter, Write};
use std::sync::Arc;

use crate::walker;

/// Options for a single `tzip jq` invocation. Mirrors the shape of
/// `QueryOpts` / `AggregateOptions` so flag handling at the CLI
/// layer stays uniform.
pub struct JqOpts {
    pub archives: Vec<String>,
    pub filter: String,
    pub include: Vec<String>,
    pub raw_output: bool,
    pub entry_prefix: bool,
    pub password: Option<String>,
    pub cache: Option<crate::indexcache::IndexCacheSpec>,
    pub quiet: bool,
    pub verbose: bool,
}

pub fn run(opts: JqOpts) -> Result<()> {
    let archives = crate::shards::expand(&opts.archives)?;
    if archives.is_empty() {
        bail!("no archives matched the given patterns");
    }
    let include: Arc<Vec<String>> = Arc::new(if opts.include.is_empty() {
        vec!["*.json".to_string(), "*.jsonl".to_string()]
    } else {
        opts.include.clone()
    });

    // Compile the filter once. jaq separates the Loader/Arena
    // (which borrow each other during compilation) from the final
    // compiled Filter struct, so we can own the latter and drop
    // the former.
    let compiled = compile_filter(&opts.filter).context("compile jq filter")?;

    let cache_arc = match &opts.cache {
        Some(spec) => Some(spec.open()?),
        None => None,
    };

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for spec in &archives {
        let mut arch = crate::reader::open_archive_with_cache(spec, cache_arc.clone())
            .with_context(|| format!("open archive {}", spec))?;
        if let Some(pw) = opts.password.as_deref() {
            arch.attach_password(pw.to_string());
        }
        for idx in 0..arch.len() {
            let name = arch.entry(idx).name.clone();
            if arch.entry(idx).uncomp_size == 0 && name.ends_with('/') {
                continue; // directory entry, skip silently
            }
            if !matches_include(&name, &include) {
                continue;
            }
            // Decompress the entry body into a Vec. Index entries
            // (`.tzip/*.v1[*.segN]`) end up matching `*.json` on
            // account of the `columnar.v1.parquet` sidecar having
            // `.parquet` — so they're naturally excluded. Any other
            // `.tzip/*` entries we pre-filter explicitly.
            if name.starts_with(".tzip/") {
                continue;
            }
            let mut body: Vec<u8> = Vec::new();
            if let Err(e) =
                arch.stream_entry_to(idx, opts.password.as_deref(), &mut body)
            {
                if opts.verbose {
                    eprintln!("tzip jq: skip {} ({}): {:#}", spec, name, e);
                }
                continue;
            }
            // Parse the body as a single JSON value. `tzip jq` on
            // non-JSON entries is a user error, but a skip-with-
            // verbose-warning is less hostile than aborting the
            // whole stream.
            let input_val = match read::parse_single(&body) {
                Ok(v) => v,
                Err(e) => {
                    if opts.verbose {
                        eprintln!(
                            "tzip jq: skip {} ({}): JSON parse failed: {:?}",
                            spec, name, e
                        );
                    }
                    continue;
                }
            };
            let prefix: Option<String> = if opts.entry_prefix {
                Some(format!("{}:{}\t", spec, name))
            } else {
                None
            };
            let ctx = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
            for value in compiled.id.run((ctx, input_val)).map(unwrap_valr) {
                let v = match value {
                    Ok(v) => v,
                    Err(e) => {
                        if opts.verbose {
                            eprintln!(
                                "tzip jq: filter error on {}:{}: {:?}",
                                spec, name, e
                            );
                        }
                        continue;
                    }
                };
                emit(&mut out, prefix.as_deref(), &v, opts.raw_output)?;
            }
        }
    }

    out.flush().ok();
    if !opts.quiet && opts.verbose {
        eprintln!("tzip jq: scanned {} archive(s)", archives.len());
    }
    Ok(())
}

/// Compile a jq filter source into the executable `jaq` form.
/// Pulled into its own fn so the lifetimes of the `Arena` and
/// `Loader` (both of which transiently borrow the filter source)
/// don't leak into `run`.
fn compile_filter(src: &str) -> Result<CompiledFilter> {
    let arena = Arena::default();
    let loader = Loader::new(
        jaq_core::defs()
            .chain(jaq_std::defs())
            .chain(jaq_json::defs()),
    );
    let modules = loader
        .load(&arena, JaqFile { code: src, path: () })
        .map_err(|errs| {
            let msg = errs
                .into_iter()
                .map(|(_, e)| format!("{:?}", e))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!("jq filter failed to parse: {}", msg)
        })?;
    let filter: CompiledFilter = Compiler::default()
        .with_funs(
            jaq_core::funs()
                .chain(jaq_std::funs())
                .chain(jaq_json::funs()),
        )
        .compile(modules)
        .map_err(|errs| {
            let msg = errs
                .into_iter()
                .map(|(_, e)| format!("{:?}", e))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!("jq filter failed to compile: {}", msg)
        })?;
    Ok(filter)
}

/// jaq's compiled-filter type is heavily generic — stash the one
/// shape we use into a type alias so the rest of the module stays
/// readable. `JustLut<Val>` is the "no streaming inputs, no custom
/// state" data profile that matches tzip's per-entry-once model.
type CompiledFilter = jaq_core::compile::Filter<jaq_core::Native<data::JustLut<Val>>>;

fn matches_include(name: &str, globs: &[String]) -> bool {
    // Reuse the walker's glob matcher. `is_excluded` returns true
    // when the name matches at least one of the given patterns —
    // the "included" test is the same semantic.
    walker::is_excluded(name, globs)
}

fn emit<W: Write>(out: &mut W, prefix: Option<&str>, v: &Val, raw: bool) -> Result<()> {
    if let Some(p) = prefix {
        out.write_all(p.as_bytes())?;
    }
    if raw {
        // jq's -r: strings print raw (no quotes, no escaping);
        // everything else serialises as JSON.
        if let Some(s) = val_as_str(v) {
            out.write_all(s.as_bytes())?;
        } else {
            write!(out, "{}", v)?;
        }
    } else {
        write!(out, "{}", v)?;
    }
    out.write_all(b"\n")?;
    Ok(())
}

/// Return the UTF-8 payload when `v` is a jaq text string
/// (`TStr`); byte strings and everything else return `None` so the
/// caller falls back to the JSON serialisation.
fn val_as_str(v: &Val) -> Option<String> {
    match v {
        Val::TStr(b) => Some(String::from_utf8_lossy(b.as_ref()).into_owned()),
        _ => None,
    }
}
