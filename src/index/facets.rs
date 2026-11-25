//! Per-field facet indices — Phase 3 of the Elastic feature-parity
//! roadmap.
//!
//! `tzip create --facet '.status_code' --facet '.user_id'` parses
//! each JSON entry whose name matches `--index-terms`'s glob, walks
//! the jq-style path (currently top-level + one-level nesting), and
//! builds a per-value counter. Stored inside the archive as one
//! entry per facet.
//!
//! `tzip aggregate --by status_code --count archive.zip` emits a
//! sorted count-by-value table in milliseconds — body-free. For any
//! archive built with `--facet '<field>'`, the aggregation is O(1)
//! disk I/O + O(distinct-values) memory.
//!
//! ## Wire format of `.tzip/facets.v1/<field>` (string-valued facet)
//!
//! ```text
//! +----+----+----+----+---+---+---+
//! | magic "TZFS" (4) | ver u16 | reserved u8 | field_len u16 |
//! +------------------+---------+-------------+---------------+
//! | field bytes                                              |
//! +----------------------------------------------------------+
//! | n_distinct u32 | total_count u64                         |
//! +----------------------------------------------------------+
//! | for each distinct value, in no particular order:         |
//! |   value_len u16 | value_bytes | count u64                |
//! +----------------------------------------------------------+
//! ```
//!
//! Value sort order is a query-time concern (top-N / histograms).
//! The index entry is a bag of `(value, count)` pairs.
//!
//! ## Field-path syntax
//!
//! v1 supports jq-style dot-paths over JSON objects and nested
//! objects only — no array indexing, no filters:
//!
//! - `.user_id` → `root.user_id`
//! - `.event.kind` → `root.event.kind`
//! - `.response.status_code` → `root.response.status_code`
//!
//! An entry where the path is missing or non-scalar contributes a
//! special `__missing__` bucket so counts remain meaningful across
//! heterogeneous corpora.

use anyhow::{bail, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

const MAGIC_STRING_FACET: [u8; 4] = *b"TZFS";
const VERSION: u16 = 1;
const MISSING_VALUE: &[u8] = b"__missing__";

/// Entry name for a facet. Spelled under `.tzip/facets.v1/` so the
/// name-index (Phase 1) and term-index (Phase 2) layouts stay
/// clean. The field is sanitised: non-ASCII-alnum / non-`[._-]` is
/// replaced with `_` to keep the resulting entry name well-formed
/// for all platforms.
pub fn entry_name_for(field: &str) -> String {
    let sanitised: String = field
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(".tzip/facets.v1/{}", sanitised.trim_start_matches('.'))
}

/// `true` when the entry name is a tzip-managed facet entry. Reader
/// paths use this to spot facet indices during archive load.
pub fn is_facet_entry_name(name: &str) -> bool {
    name.starts_with(".tzip/facets.v1/")
}

/// Field-path components — a vec of segments. `.foo.bar` → `["foo", "bar"]`.
#[derive(Debug, Clone)]
pub struct FieldPath(Vec<String>);

impl FieldPath {
    pub fn parse(spec: &str) -> Result<Self> {
        let trimmed = spec.strip_prefix('.').unwrap_or(spec);
        if trimmed.is_empty() {
            bail!("empty field path");
        }
        let segs: Vec<String> = trimmed.split('.').map(|s| s.to_string()).collect();
        if segs.iter().any(|s| s.is_empty()) {
            bail!("field path has empty segment: {:?}", spec);
        }
        Ok(Self(segs))
    }

    /// Short name derived from the final path segment, used as the
    /// facet's identifier on disk and in `--by`.
    pub fn short_name(&self) -> &str {
        self.0.last().map(|s| s.as_str()).unwrap_or("")
    }

    /// Iterator over path segments (`.a.b.c` → `["a", "b", "c"]`),
    /// used by the columnar sidecar's typed path walker.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|s| s.as_str())
    }

    /// Walk the path through `value`, returning the scalar at the
    /// leaf as owned bytes. Nested non-scalar nodes, missing paths,
    /// or null values return `None` → the builder tallies a
    /// `__missing__` bucket.
    pub fn lookup<'a>(&self, value: &'a serde_json::Value) -> Option<Vec<u8>> {
        let mut cur = value;
        for seg in &self.0 {
            cur = cur.get(seg)?;
        }
        match cur {
            serde_json::Value::String(s) => Some(s.as_bytes().to_vec()),
            serde_json::Value::Number(n) => Some(n.to_string().into_bytes()),
            serde_json::Value::Bool(b) => Some(if *b { b"true".to_vec() } else { b"false".to_vec() }),
            serde_json::Value::Null => None,
            // Objects / arrays: not a scalar; count as missing.
            _ => None,
        }
    }
}

/// Build-side accumulator for one facet.
pub struct FacetBuilder {
    path: FieldPath,
    counts: HashMap<Vec<u8>, u64>,
    total: u64,
}

impl FacetBuilder {
    pub fn new(spec: &str) -> Result<Self> {
        Ok(Self {
            path: FieldPath::parse(spec)?,
            counts: HashMap::new(),
            total: 0,
        })
    }

    pub fn field_name(&self) -> &str {
        self.path.short_name()
    }

    /// Consume one entry body, extract the field, increment the
    /// corresponding counter. Handles both single-record JSON and
    /// NDJSON (one JSON value per line) uniformly via
    /// `serde_json`'s stream deserialiser — a `.jsonl` log with
    /// 10,000 rows now contributes 10,000 counts, not 1.
    /// Malformed JSON or missing paths go to the `__missing__`
    /// bucket. `total` counts emitted records, not entries, so
    /// `aggregate` reports match the record-level reality.
    pub fn add_body(&mut self, body: &[u8]) {
        let mut any_record = false;
        let stream = serde_json::Deserializer::from_slice(body).into_iter::<serde_json::Value>();
        for value in stream {
            match value {
                Ok(v) => {
                    any_record = true;
                    self.total += 1;
                    let bucket = self
                        .path
                        .lookup(&v)
                        .unwrap_or_else(|| MISSING_VALUE.to_vec());
                    *self.counts.entry(bucket).or_insert(0) += 1;
                }
                Err(_) => {
                    // Malformed record (or malformed byte somewhere
                    // in the middle of an otherwise-good NDJSON
                    // stream). Count once as `__missing__` and
                    // stop iterating — serde_json won't recover
                    // past a parse error.
                    self.total += 1;
                    *self.counts.entry(MISSING_VALUE.to_vec()).or_insert(0) += 1;
                    return;
                }
            }
        }
        // Empty body: count a single `__missing__` so entry count
        // and record count agree on empty inputs.
        if !any_record {
            self.total += 1;
            *self.counts.entry(MISSING_VALUE.to_vec()).or_insert(0) += 1;
        }
    }

    /// Serialise as a `.tzip/facets.v1/<field>` payload.
    pub fn finish(self) -> Result<Vec<u8>> {
        let field = self.path.short_name();
        let mut out: Vec<u8> = Vec::with_capacity(32 + self.counts.len() * 24);
        out.write_all(&MAGIC_STRING_FACET)?;
        out.write_u16::<LittleEndian>(VERSION)?;
        out.write_u8(0)?; // reserved
        if field.len() > u16::MAX as usize {
            bail!("facet field name too long: {} bytes", field.len());
        }
        out.write_u16::<LittleEndian>(field.len() as u16)?;
        out.write_all(field.as_bytes())?;
        out.write_u32::<LittleEndian>(self.counts.len() as u32)?;
        out.write_u64::<LittleEndian>(self.total)?;
        for (value, count) in &self.counts {
            if value.len() > u16::MAX as usize {
                bail!("facet value too long: {} bytes", value.len());
            }
            out.write_u16::<LittleEndian>(value.len() as u16)?;
            out.write_all(value)?;
            out.write_u64::<LittleEndian>(*count)?;
        }
        Ok(out)
    }
}

/// Read-side parsed facet.
#[derive(Debug, Clone)]
pub struct Facet {
    pub field: String,
    pub total: u64,
    pub counts: HashMap<Vec<u8>, u64>,
}

impl Facet {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cur = Cursor::new(payload);
        let mut magic = [0u8; 4];
        cur.read_exact(&mut magic).context("read facet magic")?;
        if magic != MAGIC_STRING_FACET {
            bail!("facet: bad magic {:?}", magic);
        }
        let ver = cur.read_u16::<LittleEndian>()?;
        if ver != VERSION {
            bail!("facet: unsupported version {}", ver);
        }
        let _reserved = cur.read_u8()?;
        let field_len = cur.read_u16::<LittleEndian>()? as usize;
        let mut field_buf = vec![0u8; field_len];
        cur.read_exact(&mut field_buf)?;
        let field = String::from_utf8(field_buf).context("facet field name not UTF-8")?;
        let n = cur.read_u32::<LittleEndian>()? as usize;
        let total = cur.read_u64::<LittleEndian>()?;
        let mut counts: HashMap<Vec<u8>, u64> = HashMap::with_capacity(n);
        for _ in 0..n {
            let vl = cur.read_u16::<LittleEndian>()? as usize;
            let mut v = vec![0u8; vl];
            cur.read_exact(&mut v)?;
            let c = cur.read_u64::<LittleEndian>()?;
            counts.insert(v, c);
        }
        Ok(Self { field, total, counts })
    }

    /// Top-N values by count, descending. Ties broken by value
    /// lexicographically so output is deterministic.
    pub fn top_n(&self, n: usize) -> Vec<(&[u8], u64)> {
        let mut v: Vec<(&[u8], u64)> =
            self.counts.iter().map(|(k, c)| (k.as_slice(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v.truncate(n);
        v
    }

    pub fn distinct(&self) -> usize {
        self.counts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_path() {
        let p = FieldPath::parse(".status_code").unwrap();
        assert_eq!(p.short_name(), "status_code");
        let p = FieldPath::parse(".event.kind").unwrap();
        assert_eq!(p.short_name(), "kind");
    }

    #[test]
    fn empty_path_rejects() {
        assert!(FieldPath::parse("").is_err());
        assert!(FieldPath::parse(".").is_err());
        assert!(FieldPath::parse("..x").is_err());
    }

    #[test]
    fn roundtrip_counts() {
        let mut b = FacetBuilder::new(".status_code").unwrap();
        b.add_body(br#"{"status_code": 200}"#);
        b.add_body(br#"{"status_code": 200}"#);
        b.add_body(br#"{"status_code": 404}"#);
        b.add_body(br#"{"status_code": 500}"#);
        b.add_body(br#"{"status_code": 200}"#);
        b.add_body(br#"{"other": "x"}"#); // missing
        b.add_body(b"not valid json");    // missing
        let blob = b.finish().unwrap();
        let f = Facet::parse(&blob).unwrap();
        assert_eq!(f.field, "status_code");
        assert_eq!(f.total, 7);
        assert_eq!(f.distinct(), 4); // "200" "404" "500" "__missing__"
        let top = f.top_n(5);
        assert_eq!(top[0], (b"200".as_ref(), 3));
    }

    #[test]
    fn nested_path_extracts() {
        let mut b = FacetBuilder::new(".event.kind").unwrap();
        b.add_body(br#"{"event":{"kind":"click"}}"#);
        b.add_body(br#"{"event":{"kind":"scroll"}}"#);
        b.add_body(br#"{"event":{"kind":"click"}}"#);
        let blob = b.finish().unwrap();
        let f = Facet::parse(&blob).unwrap();
        let top = f.top_n(2);
        assert_eq!(top[0], (b"click".as_ref(), 2));
        assert_eq!(top[1], (b"scroll".as_ref(), 1));
    }

    #[test]
    fn ndjson_body_expands_into_many_records() {
        let mut b = FacetBuilder::new(".status_code").unwrap();
        // A single entry carrying three JSON records, one per line.
        b.add_body(
            b"{\"status_code\":200}\n{\"status_code\":200}\n{\"status_code\":500}\n",
        );
        let blob = b.finish().unwrap();
        let f = Facet::parse(&blob).unwrap();
        // 3 records total; 2 of status 200, 1 of 500.
        assert_eq!(f.total, 3);
        assert_eq!(*f.counts.get(b"200".as_ref()).unwrap(), 2);
        assert_eq!(*f.counts.get(b"500".as_ref()).unwrap(), 1);
    }

    #[test]
    fn ndjson_body_without_trailing_newline_still_works() {
        let mut b = FacetBuilder::new(".status_code").unwrap();
        b.add_body(b"{\"status_code\":200}\n{\"status_code\":500}");
        let blob = b.finish().unwrap();
        let f = Facet::parse(&blob).unwrap();
        assert_eq!(f.total, 2);
    }

    #[test]
    fn single_record_body_still_counts_as_one() {
        // Pre-fix behaviour should hold for standard single-JSON
        // entries — regression guard against double-counting the
        // lone record as "no-newline NDJSON" or similar.
        let mut b = FacetBuilder::new(".status_code").unwrap();
        b.add_body(br#"{"status_code":200}"#);
        let blob = b.finish().unwrap();
        let f = Facet::parse(&blob).unwrap();
        assert_eq!(f.total, 1);
    }

    #[test]
    fn missing_field_bucket_catches_gaps() {
        let mut b = FacetBuilder::new(".optional").unwrap();
        b.add_body(br#"{"optional": "yes"}"#);
        b.add_body(br#"{"else": "nothing"}"#);
        b.add_body(br#"{"optional": null}"#);
        let blob = b.finish().unwrap();
        let f = Facet::parse(&blob).unwrap();
        assert_eq!(*f.counts.get(b"yes".as_ref()).unwrap(), 1);
        assert_eq!(*f.counts.get(MISSING_VALUE).unwrap(), 2);
    }

    #[test]
    fn entry_name_sanitises_path() {
        assert_eq!(entry_name_for(".status_code"), ".tzip/facets.v1/status_code");
        assert_eq!(entry_name_for(".event.kind"), ".tzip/facets.v1/event.kind");
        assert_eq!(entry_name_for(".a b/c"), ".tzip/facets.v1/a_b_c");
    }
}
