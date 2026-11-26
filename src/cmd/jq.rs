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
//! Archives are processed one at a time, but entries **inside** an
//! archive fan out across `--inner-jobs` workers (default 8), each
//! with its own backing source so remote Range GETs overlap and
//! local body-decode uses N cores. The compiled filter is shared
//! across workers via `Arc` so compilation happens exactly once.
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

use anyhow::{anyhow, bail, Context, Result};
use jaq_core::load::{Arena, File as JaqFile, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::{read, Val};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::reader::{
    hint_from_archive, open_archive_with_cache, open_source_for_with_hint, SourceHint,
};
use crate::walker;
use crate::zipreader::{stream_entry_source_to, ReadEntry};

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
    pub inner_jobs: usize,
    pub cache: Option<crate::indexcache::IndexCacheSpec>,
    pub strict: bool,
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

    // Compile the filter once, share across all workers in all
    // archives. The jaq Loader+Arena transiently borrow the filter
    // source during compilation; the final CompiledFilter owns
    // everything it needs.
    let compiled = Arc::new(compile_filter(&opts.filter).context("compile jq filter")?);

    let cache_arc = match &opts.cache {
        Some(spec) => Some(spec.open()?),
        None => None,
    };

    let total_scanned = AtomicUsize::new(0);
    let report = crate::report::RunReport::new();

    for spec in &archives {
        let mut arch = open_archive_with_cache(spec, cache_arc.clone())
            .with_context(|| format!("open archive {}", spec))?;
        if let Some(pw) = opts.password.as_deref() {
            arch.attach_password(pw.to_string());
        }

        // Collect the entry indices this archive should hand to
        // workers. Skip directory entries and `.tzip/*` sentinels
        // up front so workers don't contend on stdout for noise.
        let mut entries: Vec<ReadEntry> = Vec::new();
        for idx in 0..arch.len() {
            let keep = {
                let e = arch.entry(idx);
                if e.name.starts_with(".tzip/") {
                    false
                } else if e.name.ends_with('/') && e.uncomp_size == 0 {
                    false
                } else {
                    matches_include(&e.name, &include)
                }
            };
            if !keep {
                continue;
            }
            arch.lfh_total_size_of(idx)?;
            entries.push(arch.entry(idx).clone());
        }
        if entries.is_empty() {
            continue;
        }
        let worker_hint = hint_from_archive(spec, &arch);
        drop(arch);

        run_archive_parallel(
            spec,
            entries,
            worker_hint,
            compiled.clone(),
            &opts,
            &total_scanned,
            &report,
        )?;
    }

    if !opts.quiet && opts.verbose {
        eprintln!(
            "tzip jq: scanned {} archive(s), {} matching entries",
            archives.len(),
            total_scanned.load(Ordering::Relaxed),
        );
    }
    if crate::report::finalize(&report, "jq", opts.strict, false) {
        bail!(
            "tzip jq: --strict mode and {} entries were skipped",
            report.skipped_count()
        );
    }
    Ok(())
}

/// Spread the entry list across `opts.inner_jobs` workers. Each
/// worker owns a stdout mutex guard only during its own emit, so
/// per-entry output stays line-atomic even though order is
/// non-deterministic across parallel workers (same contract as
/// `tzip grep` / `tzip hash`).
fn run_archive_parallel(
    spec: &str,
    entries: Vec<ReadEntry>,
    worker_hint: Option<SourceHint>,
    compiled: Arc<CompiledFilter>,
    opts: &JqOpts,
    total_scanned: &AtomicUsize,
    report: &crate::report::RunReport,
) -> Result<()> {
    let parallel = opts.inner_jobs.max(1).min(entries.len());

    let (tx, rx) = crossbeam_channel::unbounded::<ReadEntry>();
    for re in entries {
        let _ = tx.send(re);
    }
    drop(tx);

    // `Mutex<io::Stdout>` because `StdoutLock` isn't `Send`. Each
    // worker takes the lock only around its own `write_all` so
    // lines stay atomic without serialising the body-decode.
    let out: Mutex<std::io::Stdout> = Mutex::new(std::io::stdout());

    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::with_capacity(parallel);
        for _ in 0..parallel {
            let rx = rx.clone();
            let compiled = compiled.clone();
            let spec_str = spec.to_string();
            let password = opts.password.clone();
            let entry_prefix = opts.entry_prefix;
            let raw_output = opts.raw_output;
            let verbose = opts.verbose;
            let out = &out;
            let total_scanned = total_scanned;
            let report = report.clone();
            let worker_hint = worker_hint.clone();
            let handle = scope.spawn(move || -> Result<()> {
                let mut source = open_source_for_with_hint(&spec_str, worker_hint)
                    .with_context(|| format!("open worker source for {}", spec_str))?;
                while let Ok(re) = rx.recv() {
                    let mut body: Vec<u8> = Vec::new();
                    if let Err(e) = stream_entry_source_to(
                        &mut *source,
                        &re,
                        password.as_deref(),
                        &mut body,
                    ) {
                        if verbose {
                            eprintln!("tzip jq: skip {} ({}): {:#}", spec_str, re.name, e);
                        }
                        report.record_skip(
                            format!("{}:{}", spec_str, re.name),
                            format!("{:#}", e),
                        );
                        continue;
                    }
                    let input_val = match read::parse_single(&body) {
                        Ok(v) => v,
                        Err(e) => {
                            if verbose {
                                eprintln!(
                                    "tzip jq: skip {} ({}): JSON parse failed: {:?}",
                                    spec_str, re.name, e
                                );
                            }
                            report.record_skip(
                                format!("{}:{}", spec_str, re.name),
                                format!("JSON parse failed: {:?}", e),
                            );
                            continue;
                        }
                    };
                    // Each output value serialises into a small
                    // per-entry buffer; the stdout lock is held
                    // only while flushing that buffer so workers
                    // don't contend during the actual filter run.
                    let mut buf: Vec<u8> = Vec::with_capacity(256);
                    let prefix_bytes: Option<Vec<u8>> = if entry_prefix {
                        Some(format!("{}:{}\t", spec_str, re.name).into_bytes())
                    } else {
                        None
                    };
                    let ctx = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
                    for value in compiled.id.run((ctx, input_val)).map(unwrap_valr) {
                        let v = match value {
                            Ok(v) => v,
                            Err(e) => {
                                if verbose {
                                    eprintln!(
                                        "tzip jq: filter error on {}:{}: {:?}",
                                        spec_str, re.name, e
                                    );
                                }
                                continue;
                            }
                        };
                        emit(&mut buf, prefix_bytes.as_deref(), &v, raw_output)?;
                    }
                    if !buf.is_empty() {
                        let guard = out.lock().expect("stdout mutex");
                        let mut handle = guard.lock();
                        handle.write_all(&buf)?;
                        drop(handle);
                        drop(guard);
                    }
                    total_scanned.fetch_add(1, Ordering::Relaxed);
                    report.incr_ok();
                }
                Ok(())
            });
            handles.push(handle);
        }
        let mut first_err: Option<anyhow::Error> = None;
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(anyhow!("jq worker thread panicked"));
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })
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

fn emit<W: Write>(out: &mut W, prefix: Option<&[u8]>, v: &Val, raw: bool) -> Result<()> {
    if let Some(p) = prefix {
        out.write_all(p)?;
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
