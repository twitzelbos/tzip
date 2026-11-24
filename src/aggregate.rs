//! `tzip aggregate` — body-free count-by-value queries against
//! facet indices built by `tzip create --facet '.field'`.

use anyhow::{bail, Result};
use std::io::Write;

use crate::cli::AggregateOptions;
use crate::reader::open_archive;

pub fn run(opts: AggregateOptions) -> Result<()> {
    let arch = open_archive(&opts.archive)?;

    if opts.fields {
        let mut out = std::io::stdout().lock();
        for name in arch.facet_fields() {
            writeln!(out, "{}", name)?;
        }
        return Ok(());
    }

    let field = opts.by.as_deref().expect("by required");
    let facet = match arch.facet(field) {
        Some(f) => f,
        None => bail!(
            "no facet `{}` in archive. Build one with `tzip create --facet '.{}'`. \
             Available: [{}]",
            field,
            field,
            arch.facet_fields().join(", ")
        ),
    };

    // Rank values. `top = 0` means "all".
    let limit = if opts.top == 0 { facet.counts.len() } else { opts.top };
    let top = facet.top_n(limit);

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
            writeln!(
                out,
                "-- {} distinct value(s), {} total entries --",
                facet.distinct(),
                facet.total
            )?;
        }
    }
    Ok(())
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
