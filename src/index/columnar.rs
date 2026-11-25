//! Columnar sidecar (Parquet) — Phase 5 of the Elastic feature-parity
//! roadmap.
//!
//! `tzip create --write-columnar schema.json` extracts declared JSON
//! fields from every entry whose name matches `--index-terms`'s glob
//! and writes them out as a Parquet entry named
//! `.tzip/columnar.v1.parquet` inside the archive. `tzip query --sql
//! "SELECT ... FROM tzip ..." archive.zip` then reads that entry,
//! stages it to a temp file, and dispatches to the system `duckdb`
//! CLI — DuckDB's SQL covers everything Elastic's aggregation DSL
//! does, often faster, and we don't have to invent a query engine.
//!
//! ## Schema file format
//!
//! One JSON file with a `fields` array, each entry giving the column
//! name, its type, and the jq-style path into the entry body:
//!
//! ```json
//! {
//!   "fields": [
//!     { "name": "ts",          "type": "string",  "path": ".ts" },
//!     { "name": "status_code", "type": "int64",   "path": ".status_code" },
//!     { "name": "user",        "type": "string",  "path": ".user" },
//!     { "name": "latency_ms",  "type": "float64", "path": ".latency_ms" },
//!     { "name": "cached",      "type": "bool",    "path": ".cached" }
//!   ]
//! }
//! ```
//!
//! Supported types: `string`, `int64`, `float64`, `bool`. Entries
//! missing a field, with a null value, or with a value that doesn't
//! coerce to the declared type land as SQL `NULL` so downstream
//! aggregates remain well-defined on heterogeneous corpora.
//!
//! ## Why external DuckDB, not bundled
//!
//! Bundled DuckDB (via the `duckdb` Rust crate with `bundled`) adds
//! ~50 MB to the binary and compiles a large C++ tree — brutal for
//! CI. Shelling out to the system `duckdb` CLI (`brew install
//! duckdb` / `apt install duckdb`) keeps `tzip` lean and lets us
//! stay on DuckDB's release train without a transitive bump.
//! `tzip query --sql` prints an actionable install hint if duckdb
//! isn't on PATH.

use anyhow::{bail, Context, Result};
use arrow_array::builder::{BooleanBuilder, Float64Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::facets::FieldPath;

/// Reserved entry name for the columnar sidecar. Lives alongside the
/// other `.tzip/*.v1*` sentinels; tools that don't understand it see
/// a plain ZIP entry and ignore it.
pub const INDEX_ENTRY_NAME: &str = ".tzip/columnar.v1.parquet";

/// `true` when the entry name is the tzip-managed columnar sidecar.
pub fn is_columnar_entry_name(name: &str) -> bool {
    name == INDEX_ENTRY_NAME
}

/// Narrow set of column types Phase 5 supports. Dates and
/// timestamps round-trip as strings — DuckDB's `CAST(col AS
/// TIMESTAMP)` handles ISO-8601 and `strptime` covers the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Int64,
    Float64,
    Bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RawField {
    name: String,
    r#type: FieldType,
    path: String,
}

#[derive(Debug, Serialize)]
struct RawSchemaSer<'a> {
    fields: Vec<RawFieldRef<'a>>,
}

#[derive(Debug, Serialize)]
struct RawFieldRef<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    r#type: FieldType,
    path: String,
}

/// Arrow-schema metadata key under which we stash the tzip
/// `ColumnarSchema` as JSON. Append-mode `tzip u` reads this back
/// so segments can be produced without the user re-specifying the
/// schema path. Any Parquet reader ignores unknown keys, so this
/// is strictly additive.
const SCHEMA_METADATA_KEY: &str = "tzip.columnar.schema.v1";

/// Parsed schema — one entry per output column.
#[derive(Debug, Clone)]
pub struct ColumnarSchema {
    pub fields: Vec<ColumnField>,
}

#[derive(Debug, Clone)]
pub struct ColumnField {
    pub name: String,
    pub ty: FieldType,
    pub path: FieldPath,
}

#[derive(Debug, Deserialize)]
struct RawSchema {
    fields: Vec<RawField>,
}

impl ColumnarSchema {
    /// Parse the user's schema.json file.
    pub fn parse(json: &[u8]) -> Result<Self> {
        let raw: RawSchema =
            serde_json::from_slice(json).context("parse columnar schema json")?;
        if raw.fields.is_empty() {
            bail!("columnar schema has no fields");
        }
        let mut fields = Vec::with_capacity(raw.fields.len());
        for f in raw.fields {
            if f.name.is_empty() {
                bail!("columnar field has empty name");
            }
            fields.push(ColumnField {
                name: f.name,
                ty: f.r#type,
                path: FieldPath::parse(&f.path).with_context(|| {
                    format!("columnar field path invalid: {:?}", f.path)
                })?,
            });
        }
        // Reject duplicate column names — Arrow accepts them but
        // SQL against duplicates is undefined. Catch it here with a
        // clear error.
        let mut seen = std::collections::HashSet::new();
        for f in &fields {
            if !seen.insert(f.name.as_str()) {
                bail!("columnar schema has duplicate column name: {}", f.name);
            }
        }
        Ok(Self { fields })
    }

    /// Phase-5c schema inference. Scan a batch of JSON entry bodies
    /// (produced by walking + matching `--index-terms` globs at
    /// create time) and derive a `ColumnarSchema` from their
    /// top-level fields. The intent is "I passed `--write-columnar`
    /// without specifying a schema; just do the sensible thing."
    ///
    /// Rules:
    ///
    /// - Only top-level scalar fields. Nested objects / arrays are
    ///   skipped — users with nested columns author a schema.json
    ///   by hand.
    /// - Type is picked by first observed scalar type per field;
    ///   subsequent samples with a conflicting scalar type widen
    ///   the type (int → float on conflict, anything → string as
    ///   the universal fallback). Null values don't vote.
    /// - Column order is the order fields first appeared in the
    ///   sample — reproducible when samples arrive in deterministic
    ///   order (which the walker arranges for).
    /// - Bodies that aren't valid JSON are skipped silently; they
    ///   just don't contribute to the inference.
    ///
    /// `--write-columnar` with zero usable samples is a user error
    /// (we can't fabricate columns out of nothing); `infer` returns
    /// an `Err` the caller surfaces as a clear "no schema could be
    /// inferred, pass one explicitly or make sure --index-terms
    /// matches JSON entries."
    pub fn infer(sample_bodies: &[&[u8]]) -> Result<Self> {
        use serde_json::Value;

        /// Per-field inference state.
        struct Field {
            insertion_order: usize,
            ty: FieldType,
        }

        let mut by_name: std::collections::HashMap<String, Field> =
            std::collections::HashMap::new();
        let mut next_order = 0usize;
        let mut usable_samples = 0usize;

        for body in sample_bodies {
            let v: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let obj = match v {
                Value::Object(m) => m,
                _ => continue, // arrays / scalars at top-level aren't rows
            };
            usable_samples += 1;
            for (k, val) in obj.iter() {
                let observed = match observed_type(val) {
                    Some(t) => t,
                    None => continue, // null / nested — doesn't contribute
                };
                match by_name.get_mut(k) {
                    Some(existing) => {
                        existing.ty = widen(existing.ty, observed);
                    }
                    None => {
                        by_name.insert(
                            k.clone(),
                            Field {
                                insertion_order: next_order,
                                ty: observed,
                            },
                        );
                        next_order += 1;
                    }
                }
            }
        }

        if usable_samples == 0 {
            bail!(
                "columnar auto-infer: no valid JSON samples. Pass \
                 `--columnar-schema SCHEMA.json` explicitly, or check \
                 that `--index-terms` matches JSON-shaped entries."
            );
        }
        if by_name.is_empty() {
            bail!(
                "columnar auto-infer: {} JSON sample(s) had no top-level \
                 scalar fields to infer from (nested-only objects aren't \
                 supported). Pass `--columnar-schema SCHEMA.json` with \
                 explicit paths.",
                usable_samples
            );
        }

        // Sort fields by first-appearance order so the generated
        // schema is deterministic across runs.
        let mut ordered: Vec<(String, Field)> = by_name.into_iter().collect();
        ordered.sort_by_key(|(_, f)| f.insertion_order);
        let mut fields = Vec::with_capacity(ordered.len());
        for (name, f) in ordered {
            // Field path = top-level `.<name>` for inferred columns.
            let path = FieldPath::parse(&format!(".{}", name))?;
            fields.push(ColumnField {
                name,
                ty: f.ty,
                path,
            });
        }
        Ok(Self { fields })
    }

    /// Arrow schema for the declared columns (all nullable) with
    /// the tzip schema JSON embedded as file-level metadata under
    /// `SCHEMA_METADATA_KEY`. Phase-5b append-mode `tzip u`
    /// reads this back so new segments reuse the exact same
    /// columns without the user re-specifying the schema path.
    fn to_arrow(&self) -> Arc<ArrowSchema> {
        let fields: Vec<Field> = self
            .fields
            .iter()
            .map(|f| {
                let dt = match f.ty {
                    FieldType::String => DataType::Utf8,
                    FieldType::Int64 => DataType::Int64,
                    FieldType::Float64 => DataType::Float64,
                    FieldType::Bool => DataType::Boolean,
                };
                Field::new(&f.name, dt, true)
            })
            .collect();
        let mut meta = std::collections::HashMap::new();
        if let Ok(json) = self.to_json() {
            meta.insert(SCHEMA_METADATA_KEY.to_string(), json);
        }
        Arc::new(ArrowSchema::new(fields).with_metadata(meta))
    }

    /// Serialise back to the same JSON shape the user authored,
    /// for embedding in Parquet file metadata.
    fn to_json(&self) -> Result<String> {
        let raw = RawSchemaSer {
            fields: self
                .fields
                .iter()
                .map(|f| RawFieldRef {
                    name: &f.name,
                    r#type: f.ty,
                    path: format!(".{}", f.path.segments().collect::<Vec<_>>().join(".")),
                })
                .collect(),
        };
        serde_json::to_string(&raw).context("serialize columnar schema to json")
    }

    /// Read a `ColumnarSchema` back out of an existing
    /// `.tzip/columnar.v1.parquet` payload by parsing its Arrow
    /// file-level metadata. Fails loudly if the payload wasn't
    /// produced by tzip or doesn't carry the schema key —
    /// append-mode uses this to avoid making the user re-pass
    /// the schema path after create.
    pub fn from_parquet_bytes(payload: &[u8]) -> Result<Self> {
        use parquet::arrow::arrow_reader::ArrowReaderMetadata;
        let meta = ArrowReaderMetadata::load(
            &bytes::Bytes::copy_from_slice(payload),
            Default::default(),
        )
        .context("open parquet sidecar for schema inspection")?;
        let arrow_schema = meta.schema();
        let json = arrow_schema
            .metadata()
            .get(SCHEMA_METADATA_KEY)
            .ok_or_else(|| anyhow::anyhow!(
                "Parquet sidecar has no `{}` metadata — was it produced by tzip ≥ phase-5b? \
                 Re-run `tzip create --write-columnar schema.json` to add it, or pass \
                 `--write-columnar schema.json` to `tzip u` so append knows the columns.",
                SCHEMA_METADATA_KEY,
            ))?;
        Self::parse(json.as_bytes())
    }
}

/// Build-side accumulator. One `ColumnBuilder` per schema field.
pub struct ColumnarBuilder {
    schema: ColumnarSchema,
    arrow_schema: Arc<ArrowSchema>,
    cols: Vec<ColumnBuilder>,
    rows: usize,
}

enum ColumnBuilder {
    String(StringBuilder),
    Int64(Int64Builder),
    Float64(Float64Builder),
    Bool(BooleanBuilder),
}

impl ColumnarBuilder {
    pub fn new(schema: ColumnarSchema) -> Self {
        let cols: Vec<ColumnBuilder> = schema
            .fields
            .iter()
            .map(|f| match f.ty {
                FieldType::String => ColumnBuilder::String(StringBuilder::new()),
                FieldType::Int64 => ColumnBuilder::Int64(Int64Builder::new()),
                FieldType::Float64 => ColumnBuilder::Float64(Float64Builder::new()),
                FieldType::Bool => ColumnBuilder::Bool(BooleanBuilder::new()),
            })
            .collect();
        let arrow_schema = schema.to_arrow();
        Self { schema, arrow_schema, cols, rows: 0 }
    }

    pub fn schema(&self) -> &ColumnarSchema {
        &self.schema
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Consume one entry body. JSON parse errors or missing paths
    /// produce `NULL` for every column of that row — the row count
    /// still advances so columns stay aligned with the sidecar's
    /// notion of "N entries contributed to the archive."
    pub fn add_body(&mut self, body: &[u8]) {
        self.rows += 1;
        let value: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                for col in &mut self.cols {
                    col.push_null();
                }
                return;
            }
        };
        for (i, col) in self.cols.iter_mut().enumerate() {
            let field = &self.schema.fields[i];
            let probed = walk_path(&value, &field.path);
            col.push_from_json(field.ty, probed);
        }
    }

    /// Serialise the accumulated columns as a Parquet payload with
    /// Snappy page compression. One row group covers everything —
    /// Phase 5 corpora are "one archive one row-group" scale; if
    /// corpora grow past ~1M rows the writer will add further row
    /// groups implicitly.
    pub fn finish(self) -> Result<Vec<u8>> {
        let arrays: Vec<ArrayRef> = self
            .cols
            .into_iter()
            .map(|c| c.finish())
            .collect();
        let batch = RecordBatch::try_new(self.arrow_schema.clone(), arrays)
            .context("build columnar record batch")?;
        let mut buf: Vec<u8> = Vec::with_capacity(1024 * 64);
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let mut w = ArrowWriter::try_new(&mut buf, self.arrow_schema.clone(), Some(props))
            .context("open parquet writer")?;
        w.write(&batch).context("write parquet row group")?;
        w.close().context("close parquet writer")?;
        Ok(buf)
    }
}

impl ColumnBuilder {
    fn push_null(&mut self) {
        match self {
            ColumnBuilder::String(b) => b.append_null(),
            ColumnBuilder::Int64(b) => b.append_null(),
            ColumnBuilder::Float64(b) => b.append_null(),
            ColumnBuilder::Bool(b) => b.append_null(),
        }
    }

    fn push_from_json(&mut self, ty: FieldType, value: Option<&serde_json::Value>) {
        use serde_json::Value;
        let Some(v) = value else {
            self.push_null();
            return;
        };
        match (ty, self, v) {
            (FieldType::String, ColumnBuilder::String(b), Value::String(s)) => b.append_value(s),
            // Permissive: coerce scalar numbers / bools to their string
            // representation so a schema that marks a numeric field as
            // string doesn't silently null everything out.
            (FieldType::String, ColumnBuilder::String(b), Value::Number(n)) => {
                b.append_value(n.to_string())
            }
            (FieldType::String, ColumnBuilder::String(b), Value::Bool(bv)) => {
                b.append_value(if *bv { "true" } else { "false" })
            }
            (FieldType::Int64, ColumnBuilder::Int64(b), Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    b.append_value(i);
                } else if let Some(f) = n.as_f64() {
                    // JSON number came through as float; truncate.
                    if f.is_finite() {
                        b.append_value(f as i64);
                    } else {
                        b.append_null();
                    }
                } else {
                    b.append_null();
                }
            }
            (FieldType::Float64, ColumnBuilder::Float64(b), Value::Number(n)) => {
                if let Some(f) = n.as_f64() {
                    b.append_value(f);
                } else {
                    b.append_null();
                }
            }
            (FieldType::Bool, ColumnBuilder::Bool(b), Value::Bool(bv)) => b.append_value(*bv),
            // Any other combination (null, object, array, mismatched
            // scalar) lands as NULL.
            (_, this, _) => this.push_null(),
        }
    }

    fn finish(self) -> ArrayRef {
        match self {
            ColumnBuilder::String(mut b) => Arc::new(b.finish()) as ArrayRef,
            ColumnBuilder::Int64(mut b) => Arc::new(b.finish()) as ArrayRef,
            ColumnBuilder::Float64(mut b) => Arc::new(b.finish()) as ArrayRef,
            ColumnBuilder::Bool(mut b) => Arc::new(b.finish()) as ArrayRef,
        }
    }
}

/// Return the scalar `FieldType` for a JSON value, or `None` for
/// null / object / array (values that shouldn't influence
/// inference). Used by `ColumnarSchema::infer`.
fn observed_type(v: &serde_json::Value) -> Option<FieldType> {
    use serde_json::Value;
    match v {
        Value::String(_) => Some(FieldType::String),
        Value::Bool(_) => Some(FieldType::Bool),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                Some(FieldType::Int64)
            } else {
                Some(FieldType::Float64)
            }
        }
        Value::Null | Value::Object(_) | Value::Array(_) => None,
    }
}

/// Widen a column's existing inferred type when a new sample
/// contradicts it. The lattice:
///
/// - Int widens to Float on first non-integer number.
/// - Any mismatch between non-number types (String ↔ Bool,
///   String ↔ Number, Bool ↔ Number) widens to String — safe
///   because every JSON scalar has a string form.
///
/// Called per-sample-per-field; cheap, no allocations.
fn widen(current: FieldType, observed: FieldType) -> FieldType {
    match (current, observed) {
        (a, b) if a == b => a,
        (FieldType::Int64, FieldType::Float64) | (FieldType::Float64, FieldType::Int64) => {
            FieldType::Float64
        }
        _ => FieldType::String,
    }
}

/// Walk `path` through `value`, returning a reference to the scalar
/// at the leaf. Nested non-scalar nodes or missing paths return
/// `None`. Mirrors `FieldPath::lookup` but returns the Value
/// reference so callers can branch on type.
fn walk_path<'a>(value: &'a serde_json::Value, path: &FieldPath) -> Option<&'a serde_json::Value> {
    let mut cur = value;
    for seg in path.segments() {
        cur = cur.get(seg)?;
    }
    match cur {
        serde_json::Value::Null => None,
        _ => Some(cur),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_parses_well_formed_json() {
        let src = br#"{
            "fields": [
                { "name": "ts",     "type": "string",  "path": ".ts" },
                { "name": "status", "type": "int64",   "path": ".status_code" },
                { "name": "lat",    "type": "float64", "path": ".latency_ms" },
                { "name": "ok",     "type": "bool",    "path": ".cached" }
            ]
        }"#;
        let s = ColumnarSchema::parse(src).unwrap();
        assert_eq!(s.fields.len(), 4);
        assert_eq!(s.fields[0].name, "ts");
        assert_eq!(s.fields[1].ty, FieldType::Int64);
    }

    #[test]
    fn schema_rejects_empty_and_duplicates() {
        assert!(ColumnarSchema::parse(br#"{"fields": []}"#).is_err());
        assert!(ColumnarSchema::parse(
            br#"{"fields": [
                {"name":"x","type":"int64","path":".x"},
                {"name":"x","type":"int64","path":".y"}
            ]}"#
        )
        .is_err());
    }

    #[test]
    fn roundtrip_basic_types_produces_valid_parquet() {
        let src = br#"{
            "fields": [
                { "name": "status", "type": "int64",   "path": ".status_code" },
                { "name": "user",   "type": "string",  "path": ".user" },
                { "name": "lat",    "type": "float64", "path": ".latency_ms" },
                { "name": "ok",     "type": "bool",    "path": ".cached" }
            ]
        }"#;
        let schema = ColumnarSchema::parse(src).unwrap();
        let mut b = ColumnarBuilder::new(schema);
        b.add_body(br#"{"status_code":200,"user":"alice","latency_ms":3.4,"cached":true}"#);
        b.add_body(br#"{"status_code":500,"user":"bob","latency_ms":12.1,"cached":false}"#);
        b.add_body(br#"{"status_code":404}"#);
        b.add_body(b"not json");
        let payload = b.finish().unwrap();

        // Parquet magic "PAR1" at both ends.
        assert_eq!(&payload[0..4], b"PAR1");
        assert_eq!(&payload[payload.len() - 4..], b"PAR1");
    }

    #[test]
    fn missing_path_lands_as_null() {
        let schema = ColumnarSchema::parse(
            br#"{"fields":[{"name":"s","type":"int64","path":".status_code"}]}"#,
        )
        .unwrap();
        let mut b = ColumnarBuilder::new(schema);
        b.add_body(br#"{"other":42}"#);
        b.add_body(br#"{"status_code":null}"#);
        b.add_body(br#"{"status_code":200}"#);
        let payload = b.finish().unwrap();
        assert_eq!(&payload[0..4], b"PAR1");
    }

    #[test]
    fn nested_path_extracts() {
        let schema = ColumnarSchema::parse(
            br#"{"fields":[{"name":"kind","type":"string","path":".event.kind"}]}"#,
        )
        .unwrap();
        let mut b = ColumnarBuilder::new(schema);
        b.add_body(br#"{"event":{"kind":"click"}}"#);
        let payload = b.finish().unwrap();
        assert_eq!(&payload[0..4], b"PAR1");
    }

    #[test]
    fn schema_roundtrips_through_parquet_metadata() {
        let src = br#"{
            "fields": [
                { "name": "ts",          "type": "string",  "path": ".ts" },
                { "name": "status_code", "type": "int64",   "path": ".status_code" },
                { "name": "latency_ms",  "type": "float64", "path": ".latency_ms" },
                { "name": "cached",      "type": "bool",    "path": ".event.cached" }
            ]
        }"#;
        let schema = ColumnarSchema::parse(src).unwrap();
        let mut b = ColumnarBuilder::new(schema);
        b.add_body(br#"{"ts":"2026-11-25","status_code":200,"latency_ms":1.2,"event":{"cached":true}}"#);
        let payload = b.finish().unwrap();

        let recovered = ColumnarSchema::from_parquet_bytes(&payload).unwrap();
        assert_eq!(recovered.fields.len(), 4);
        assert_eq!(recovered.fields[0].name, "ts");
        assert_eq!(recovered.fields[0].ty, FieldType::String);
        assert_eq!(recovered.fields[1].ty, FieldType::Int64);
        assert_eq!(recovered.fields[2].ty, FieldType::Float64);
        assert_eq!(recovered.fields[3].ty, FieldType::Bool);
        // Nested path preserved end-to-end.
        let last = recovered.fields.last().unwrap();
        let path: Vec<&str> = last.path.segments().collect();
        assert_eq!(path, vec!["event", "cached"]);
    }

    #[test]
    fn infer_picks_types_and_preserves_first_seen_order() {
        let samples: [&[u8]; 3] = [
            br#"{"ts":"2026-11-25","status_code":200,"latency_ms":1.2,"cached":true}"#,
            br#"{"ts":"2026-11-25","status_code":500,"latency_ms":9.9,"cached":false}"#,
            br#"{"ts":"2026-11-25","status_code":404,"latency_ms":3.1,"cached":true}"#,
        ];
        let schema = ColumnarSchema::infer(&samples).unwrap();
        // Order mirrors first-appearance.
        let names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["ts", "status_code", "latency_ms", "cached"]);
        assert_eq!(schema.fields[0].ty, FieldType::String);
        assert_eq!(schema.fields[1].ty, FieldType::Int64);
        assert_eq!(schema.fields[2].ty, FieldType::Float64);
        assert_eq!(schema.fields[3].ty, FieldType::Bool);
    }

    #[test]
    fn infer_widens_int_to_float_when_sample_has_decimal() {
        let samples: [&[u8]; 2] = [
            br#"{"latency":10}"#,
            br#"{"latency":10.5}"#,
        ];
        let schema = ColumnarSchema::infer(&samples).unwrap();
        assert_eq!(schema.fields[0].ty, FieldType::Float64);
    }

    #[test]
    fn infer_falls_back_to_string_on_mixed_types() {
        let samples: [&[u8]; 2] = [
            br#"{"status":"ok"}"#,
            br#"{"status":200}"#,
        ];
        let schema = ColumnarSchema::infer(&samples).unwrap();
        assert_eq!(schema.fields[0].ty, FieldType::String);
    }

    #[test]
    fn infer_skips_nested_and_null_fields() {
        let samples: [&[u8]; 2] = [
            br#"{"ts":"t","nested":{"x":1},"maybe":null,"n":1}"#,
            br#"{"ts":"t","nested":{"x":2},"maybe":null,"n":2}"#,
        ];
        let schema = ColumnarSchema::infer(&samples).unwrap();
        let names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
        // `nested` + `maybe` get dropped; only scalar `ts` + `n` survive.
        assert_eq!(names, vec!["ts", "n"]);
    }

    #[test]
    fn infer_errors_when_no_usable_samples() {
        let samples: [&[u8]; 2] = [b"not json", b"[]"];
        let err = ColumnarSchema::infer(&samples).unwrap_err();
        assert!(format!("{}", err).contains("no valid JSON samples"));
    }

    #[test]
    fn infer_errors_on_null_only_fields() {
        // JSON parses OK but every field is null / nested → no
        // top-level scalars to infer from.
        let samples: [&[u8]; 1] = [br#"{"x":null,"y":[1,2]}"#];
        let err = ColumnarSchema::infer(&samples).unwrap_err();
        assert!(format!("{}", err).contains("no top-level scalar fields"));
    }

    #[test]
    fn from_parquet_bytes_rejects_payload_without_schema_metadata() {
        // Minimal valid Parquet file produced WITHOUT going through
        // `ColumnarSchema::to_arrow` — simulates a sidecar built by
        // another tool.
        use arrow_array::Int64Array;
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::WriterProperties;
        use std::sync::Arc;
        let arrow_schema = Arc::new(ArrowSchema::new(vec![Field::new("x", DataType::Int64, true)]));
        let batch = arrow_array::RecordBatch::try_new(
            arrow_schema.clone(),
            vec![Arc::new(Int64Array::from(vec![Some(1)]))],
        )
        .unwrap();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(
            &mut buf,
            arrow_schema.clone(),
            Some(WriterProperties::builder().build()),
        )
        .unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let err = ColumnarSchema::from_parquet_bytes(&buf).unwrap_err();
        assert!(
            format!("{}", err).contains(SCHEMA_METADATA_KEY),
            "error should name the missing metadata key: {}",
            err
        );
    }
}
