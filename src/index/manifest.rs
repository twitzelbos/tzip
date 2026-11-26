//! `.tzip/manifest.v1` — single authoritative record of which index
//! components this archive carries.
//!
//! Before this module, each of `tzip grep` / `tzip aggregate` /
//! `tzip query` / `tzip jq` discovered its sidecar by walking the CDR
//! looking for a name prefix (`.tzip/names.v1`, `.tzip/terms.v1*`,
//! `.tzip/facets.v1/<f>[.segN]`, `.tzip/columnar.v1.parquet[.segN]`).
//! That worked, but the knowledge of "what sentinels exist and in
//! what versions" lived nowhere canonical — four callers, four
//! discovery routines. A future index addition had to touch every one.
//!
//! The manifest flips that: `tzip create` writes a single
//! `.tzip/manifest.v1` listing everything that's present, readers
//! consult it (via `Archive::manifest()`) for a one-shot answer, and
//! CDR-walk discovery stays as the fallback when the manifest is
//! absent or stale.
//!
//! ## Wire format
//!
//! Hand-rolled key/value text — no new crate dep, trivially hand-
//! inspectable with `tzip cat archive.zip .tzip/manifest.v1`. Each
//! line is `key = value`; `#` prefixes a comment; blank lines are
//! ignored. Keys with dotted paths represent nested state (e.g.
//! `facets.status_code = 3` means the `status_code` facet has 3
//! segments).
//!
//! ```text
//! # tzip index manifest
//! schema = 1
//! names = 1
//! terms = 2
//! columnar = 1
//! facets.status_code = 3
//! facets.user = 1
//! ```
//!
//! ## Backward-compatibility model
//!
//! - Older tzip binaries ignore the manifest sentinel because it's
//!   just another `.tzip/*` entry (which readers already skip from
//!   user-facing output).
//! - Newer readers that find no manifest fall back to the CDR-walk
//!   discovery path — no regression.
//! - Newer readers that find a manifest trust it as a fast
//!   capability check (`has_terms()`, etc.) but **do NOT** use it to
//!   short-circuit the actual segment-loading code paths yet: the
//!   segment-loader still reads from the CDR entries directly. That
//!   way, a stale manifest (e.g. one left over from a previous
//!   `tzip u` that didn't rewrite it — PR-for-later) can't cause a
//!   reader to miss an index component.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;

/// Reserved sentinel entry name. Stable across versions.
pub const MANIFEST_ENTRY_NAME: &str = ".tzip/manifest.v1";

/// Schema version this writer emits. Readers that see a higher
/// schema should still parse the keys they recognise and fall
/// back on anything they don't.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    /// Number of `.tzip/names.v1[.segN]` sentinels present.
    /// `0` → no name index.
    pub names_segments: u32,
    /// Number of `.tzip/terms.v1[.segN]` sentinels present.
    pub terms_segments: u32,
    /// Number of `.tzip/columnar.v1.parquet[.segN]` sentinels present.
    pub columnar_segments: u32,
    /// Per-facet-field segment counts, keyed by the facet's leaf
    /// name (the part after `.tzip/facets.v1/`). BTreeMap for a
    /// stable serialisation order — determinism makes
    /// byte-identical builds possible when the rest of the
    /// archive is deterministic too.
    pub facets: BTreeMap<String, u32>,
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            ..Self::default()
        }
    }

    pub fn has_names(&self) -> bool {
        self.names_segments > 0
    }

    pub fn has_terms(&self) -> bool {
        self.terms_segments > 0
    }

    pub fn has_columnar(&self) -> bool {
        self.columnar_segments > 0
    }

    pub fn facet_fields(&self) -> impl Iterator<Item = &str> {
        self.facets.keys().map(|s| s.as_str())
    }

    /// Serialise to the canonical text form. Deterministic given
    /// the same fields (BTreeMap for facets, fixed line order).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("# tzip index manifest\n");
        out.push_str(&format!("schema = {}\n", self.schema));
        if self.names_segments > 0 {
            out.push_str(&format!("names = {}\n", self.names_segments));
        }
        if self.terms_segments > 0 {
            out.push_str(&format!("terms = {}\n", self.terms_segments));
        }
        if self.columnar_segments > 0 {
            out.push_str(&format!("columnar = {}\n", self.columnar_segments));
        }
        for (field, n) in &self.facets {
            // Field names come straight from the user's
            // `--facet '.path.leaf'` so they may contain dots.
            // Reject anything that'd break the key = value shape
            // instead of writing a parse-ambiguous manifest.
            if field.contains('=') || field.contains('\n') || field.contains('\r') {
                continue;
            }
            out.push_str(&format!("facets.{} = {}\n", field, n));
        }
        out.into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .context("manifest is not valid UTF-8")?;
        let mut m = Manifest::new();
        for (ln, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("manifest line {} missing '=': {:?}", ln + 1, raw_line))?;
            let key = k.trim();
            let val = v.trim();
            match key {
                "schema" => {
                    m.schema = val.parse::<u32>()
                        .with_context(|| format!("schema not u32 on line {}", ln + 1))?;
                }
                "names" => m.names_segments = parse_u32(val, ln)?,
                "terms" => m.terms_segments = parse_u32(val, ln)?,
                "columnar" => m.columnar_segments = parse_u32(val, ln)?,
                other if other.starts_with("facets.") => {
                    let field = &other["facets.".len()..];
                    if field.is_empty() {
                        bail!("manifest line {}: empty facet field name", ln + 1);
                    }
                    m.facets.insert(field.to_string(), parse_u32(val, ln)?);
                }
                _ => {
                    // Unknown key: forward-compat — a newer writer
                    // may have added a key this reader doesn't
                    // understand. Skip rather than fail.
                    continue;
                }
            }
        }
        Ok(m)
    }
}

fn parse_u32(val: &str, ln: usize) -> Result<u32> {
    val.parse::<u32>()
        .with_context(|| format!("expected u32 on manifest line {}: {:?}", ln + 1, val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let mut m = Manifest::new();
        m.names_segments = 2;
        m.terms_segments = 1;
        m.columnar_segments = 3;
        m.facets.insert("status_code".into(), 4);
        m.facets.insert("user".into(), 1);

        let bytes = m.encode();
        let back = Manifest::decode(&bytes).expect("decode");
        assert_eq!(m, back);
    }

    #[test]
    fn decode_ignores_unknown_keys_and_comments() {
        let input = b"\
# a comment
schema = 1
names = 0
future_field = whatever
terms = 2
# trailing comment
";
        let m = Manifest::decode(input).unwrap();
        assert_eq!(m.schema, 1);
        assert_eq!(m.names_segments, 0);
        assert_eq!(m.terms_segments, 2);
    }

    #[test]
    fn decode_rejects_garbage() {
        let input = b"schema = not-a-number\n";
        assert!(Manifest::decode(input).is_err());
    }

    #[test]
    fn encode_skips_zero_counts() {
        let m = Manifest::new();
        let s = String::from_utf8(m.encode()).unwrap();
        // Only header + schema line, nothing else.
        assert!(s.contains("schema = 1"));
        assert!(!s.contains("names ="));
        assert!(!s.contains("terms ="));
        assert!(!s.contains("facets."));
    }

    #[test]
    fn capability_methods() {
        let mut m = Manifest::new();
        assert!(!m.has_names());
        assert!(!m.has_terms());
        assert!(!m.has_columnar());
        assert!(m.facet_fields().next().is_none());
        m.names_segments = 1;
        m.terms_segments = 2;
        m.columnar_segments = 3;
        m.facets.insert("x".into(), 1);
        assert!(m.has_names());
        assert!(m.has_terms());
        assert!(m.has_columnar());
        assert_eq!(m.facet_fields().collect::<Vec<_>>(), vec!["x"]);
    }
}
