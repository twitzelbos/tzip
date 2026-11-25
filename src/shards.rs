//! Shard-glob expansion — shared by `tzip grep`, `tzip aggregate`,
//! `tzip query`.
//!
//! Local shell globs (`shard-*.zip` on your terminal) are expanded
//! by your shell before argv lands here, so we only expand the
//! schemes shells can't help with — specifically `s3://bucket/path/*.zip`
//! patterns, resolved via S3's `ListObjectsV2`.
//!
//! Phase 7 of the Elastic feature-parity roadmap uses this to let
//! `tzip aggregate --by status_code s3://logs/shards/*.zip` fan out
//! across every matching shard in parallel and merge the results —
//! the "horizontal shard parallelism" row of the parity table.

use anyhow::{Context, Result};

use crate::sources::s3_list;

/// Expand each spec into one or more concrete archive URLs/paths.
/// `s3://bucket/prefix/*.zip` turns into every matching S3 object;
/// everything else passes through unchanged (shell globs for local
/// files are assumed already expanded).
pub fn expand(input: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for spec in input {
        if let Some(rest) = spec.strip_prefix("s3://") {
            if s3_list::has_glob(rest) {
                let expanded = s3_list::expand(rest)
                    .with_context(|| format!("expand S3 glob {}", spec))?;
                out.extend(expanded);
                continue;
            }
        }
        out.push(spec.clone());
    }
    Ok(out)
}
