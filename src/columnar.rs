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
use serde::Deserialize;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Int64,
    Float64,
    Bool,
}

#[derive(Debug, Clone, Deserialize)]
struct RawField {
    name: String,
    r#type: FieldType,
    path: String,
}

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

    /// Arrow schema for the declared columns; all nullable.
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
        Arc::new(ArrowSchema::new(fields))
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
}
