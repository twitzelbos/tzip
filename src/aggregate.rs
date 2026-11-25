//! `tzip aggregate` — body-free count-by-value queries against
//! facet indices built by `tzip create --facet '.field'`.
//!
//! Phase 7 adds shard fan-out: pass multiple archives (or a
//! `s3://bucket/shards/*.zip` glob) and counts are summed across
//! all of them. The merge is identical to the per-archive segment
//! merge from Phase 4 — a `Facet` is just a bag of
//! `(value, count)` pairs, and adding two bags is addition on
//! each key.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::cli::AggregateOptions;
use crate::facets::Facet;
use crate::reader::open_archive_with_cache;

pub fn run(opts: AggregateOptions) -> Result<()> {
    let cache = match &opts.cache {
        Some(spec) => Some(spec.open()?),
        None => None,
    };
    let archives = crate::shards::expand(&opts.archives)?;
    if archives.is_empty() {
        bail!("no archives matched the given patterns");
    }

    // `--fields`: list the facet fields the first shard declares.
    // Shards are expected to be homogeneous — different schemas
    // across shards is an upstream error, not something aggregate
    // tries to patch over.
    if opts.fields {
        let arch = open_archive_with_cache(&archives[0], cache.clone())
            .with_context(|| format!("open archive {}", archives[0]))?;
        let mut out = std::io::stdout().lock();
        for name in arch.facet_fields() {
            writeln!(out, "{}", name)?;
        }
        return Ok(());
    }

    let field = opts.by.as_deref().expect("by required");
    let merged = merge_facet_across_shards(&archives, field, cache, opts.verbose)?;

    let limit = if opts.top == 0 { merged.counts.len() } else { opts.top };
    let top = merged.top_n(limit);

    let mut out = std::io::stdout().lock();
    if opts.json {
        for (value, count) in top {
            writeln!(
                out,
                "{{\"value\":{},\"count\":{}}}",
                json_str_from_bytes(value),
                count
            )?;
        }
    } else {
        for (value, count) in top {
            writeln!(
                out,
                "{:>12}  {}",
                count,
                String::from_utf8_lossy(value)
            )?;
        }
        if !opts.quiet {
            if archives.len() > 1 {
                writeln!(
                    out,
                    "-- {} distinct value(s), {} total entries across {} shards --",
                    merged.distinct(),
                    merged.total,
                    archives.len(),
                )?;
            } else {
                writeln!(
                    out,
                    "-- {} distinct value(s), {} total entries --",
                    merged.distinct(),
                    merged.total,
                )?;
            }
        }
    }
    Ok(())
}

/// Open every shard and sum its `facet(field)` into one `Facet`.
/// Each archive's `facet()` already merges its internal Phase-4
/// `.segN` segments; here we add one more layer of summation
/// across shards.
fn merge_facet_across_shards(
    archives: &[String],
    field: &str,
    cache: Option<Arc<crate::indexcache::IndexCache>>,
    verbose: bool,
) -> Result<Facet> {
    let mut merged_counts: HashMap<Vec<u8>, u64> = HashMap::new();
    let mut merged_total: u64 = 0;
    let mut seen_field: Option<String> = None;
    // Only reported in the "no shard had the facet" error — collect
    // the available-field set of the first archive so the user has
    // a hint of what fields *are* indexed.
    let mut available_hint: Vec<String> = Vec::new();
    for spec in archives {
        if verbose {
            eprintln!("tzip aggregate: shard {}", spec);
        }
        let arch = open_archive_with_cache(spec, cache.clone())
            .with_context(|| format!("open archive {}", spec))?;
        if available_hint.is_empty() {
            available_hint = arch.facet_fields();
        }
        match arch.facet(field) {
            Some(facet) => {
                if seen_field.is_none() {
                    seen_field = Some(facet.field.clone());
                }
                merged_total += facet.total;
                for (v, c) in facet.counts {
                    *merged_counts.entry(v).or_insert(0) += c;
                }
            }
            None => {
                // Skip shards without this facet — a corpus where
                // only some shards declared `--facet '.field'` is
                // still a valid merge; the empty-sum shards just
                // don't contribute. But if NO shard has it, we
                // error at the end.
                if verbose {
                    eprintln!(
                        "tzip aggregate: shard {} has no facet `{}`, skipping",
                        spec, field
                    );
                }
            }
        }
    }
    if seen_field.is_none() {
        bail!(
            "no facet `{}` in {} shard(s). Build one with `tzip create --facet '.{}'`. \
             Available on first shard: [{}]",
            field,
            archives.len(),
            field,
            available_hint.join(", ")
        );
    }
    Ok(Facet {
        field: seen_field.unwrap(),
        total: merged_total,
        counts: merged_counts,
    })
}

/// Minimal JSON string escaper for a byte-slice value. Mirrors the
/// one in `hash.rs`; non-UTF-8 falls back to `�`.
fn json_str_from_bytes(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
