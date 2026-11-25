//! Row-group pruning for `tzip query --sql` — ChatGPT review #4.
//!
//! Before shelling out to DuckDB, we parse a *minimal* subset of
//! the user's SQL to extract conjunctive `col OP constant`
//! predicates. We then read only the Parquet footer of each
//! shard's columnar sidecar (tens of KB, not the entire multi-MB
//! body), look at each row group's min/max statistics, and skip
//! row groups whose stats prove the predicate can never match.
//! A composed Parquet containing only the surviving row groups
//! is staged to the temp file DuckDB reads.
//!
//! ## Safety / soundness
//!
//! The pruning is a pure *speed* optimisation. It is sound iff
//! we never drop a row group that could contain a matching row.
//! Rules we hold to:
//!
//! - If we can't parse the SQL into a flat `WHERE a AND b AND …`
//!   of `col OP constant` terms, return `None` → full-extract
//!   fallback.
//! - If a column referenced in a predicate is not in the Parquet
//!   schema, bail out.
//! - If a row group's statistics for the predicate column are
//!   missing, assume the row group matches (don't prune).
//! - If a row group's min/max window overlaps the predicate's
//!   value range, keep the row group.
//! - DuckDB still runs the full WHERE on whatever we hand it, so
//!   even a mistakenly kept row group only costs a bit of CPU —
//!   never wrong results.
//!
//! What we explicitly *don't* try to handle in the parser:
//!
//! - `OR`, `NOT`, `IN (...)`, `BETWEEN`, subqueries
//! - Joins (`FROM a, b`, `JOIN`)
//! - Aggregates in the WHERE (functions on column values)
//! - String operators beyond `=` / `<>` / `!=`
//!
//! Anything the parser can't handle → `None` → fall back.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use parquet::file::statistics::Statistics;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Comparison operator in a `col OP value` predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Right-hand side of a `col OP value` predicate. Matches the
/// scalar types tzip's columnar schema supports (plus `Bool`,
/// which arrives as a literal).
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub column: String,
    pub op: Op,
    pub value: Literal,
}

/// Parse the user's SQL and extract a conjunctive list of
/// `col OP constant` predicates from its WHERE clause, if the
/// whole statement is flat enough that we can be sure every row
/// the query cares about is covered by them.
///
/// Returns:
/// - `Some(preds)` when the SQL is a simple
///   `SELECT ... FROM tzip [WHERE pred1 AND pred2 …]` and every
///   WHERE term reduces to `col OP constant`.
/// - `None` otherwise (anything with OR / NOT / parentheses /
///   joins / subqueries / unsupported ops / non-literal values).
///
/// Only called as a *filter*: any `None` just means we skip
/// pruning for this query — DuckDB still runs on the full
/// sidecar. Soundness bugs here translate to missing data in the
/// query result, so this parser is deliberately conservative: it
/// returns `None` whenever a token doesn't fit the restricted
/// grammar, never guesses.
pub fn extract_conjunctive_predicates(sql: &str) -> Option<Vec<Predicate>> {
    // Lowercase-then-tokenize approach, but preserve string
    // literal case. The tokenizer returns a sequence of
    // (kind, text) pairs.
    let tokens = tokenize(sql)?;
    let mut p = Parser::new(&tokens);
    p.parse_statement()
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    QuotedIdent(String),
    Int(i64),
    Float(f64),
    Str(String),
    Keyword(Keyword),
    Op(Op),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Star,
    Dot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Select,
    From,
    Where,
    And,
    Or,
    Not,
    True,
    False,
    Null,
    In,
    Between,
    Like,
    Is,
    Limit,
    Order,
    Group,
    As,
}

fn tokenize(sql: &str) -> Option<Vec<Tok>> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut out: Vec<Tok> = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            // Line comment — advance to EOL.
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            // Block comment.
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if b == b',' {
            out.push(Tok::Comma);
            i += 1;
            continue;
        }
        if b == b';' {
            out.push(Tok::Semicolon);
            i += 1;
            continue;
        }
        if b == b'(' {
            out.push(Tok::LParen);
            i += 1;
            continue;
        }
        if b == b')' {
            out.push(Tok::RParen);
            i += 1;
            continue;
        }
        if b == b'.' {
            out.push(Tok::Dot);
            i += 1;
            continue;
        }
        if b == b'*' {
            out.push(Tok::Star);
            i += 1;
            continue;
        }
        if b == b'=' {
            out.push(Tok::Op(Op::Eq));
            i += 1;
            continue;
        }
        if b == b'<' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                out.push(Tok::Op(Op::Le));
                i += 2;
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                out.push(Tok::Op(Op::Ne));
                i += 2;
            } else {
                out.push(Tok::Op(Op::Lt));
                i += 1;
            }
            continue;
        }
        if b == b'>' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                out.push(Tok::Op(Op::Ge));
                i += 2;
            } else {
                out.push(Tok::Op(Op::Gt));
                i += 1;
            }
            continue;
        }
        if b == b'!' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                out.push(Tok::Op(Op::Ne));
                i += 2;
                continue;
            }
            return None;
        }
        if b == b'\'' {
            // String literal with SQL doubled-quote escapes.
            let mut s = String::new();
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    out.push(Tok::Str(s));
                    break;
                }
                s.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }
        if b == b'"' {
            // Double-quoted identifier.
            let mut s = String::new();
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                s.push(bytes[i] as char);
                i += 1;
            }
            i += 1;
            out.push(Tok::QuotedIdent(s));
            continue;
        }
        if b.is_ascii_digit() || (b == b'-' && out.last().map_or(true, is_literal_start)) {
            // Numeric literal (signed int or float).
            let start = i;
            if b == b'-' {
                i += 1;
            }
            let mut saw_dot = false;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_digit() {
                    i += 1;
                } else if c == b'.' && !saw_dot {
                    saw_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            let text = std::str::from_utf8(&bytes[start..i]).ok()?;
            if saw_dot {
                out.push(Tok::Float(text.parse().ok()?));
            } else {
                out.push(Tok::Int(text.parse().ok()?));
            }
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = std::str::from_utf8(&bytes[start..i]).ok()?;
            let lower = word.to_ascii_lowercase();
            out.push(match lower.as_str() {
                "select" => Tok::Keyword(Keyword::Select),
                "from" => Tok::Keyword(Keyword::From),
                "where" => Tok::Keyword(Keyword::Where),
                "and" => Tok::Keyword(Keyword::And),
                "or" => Tok::Keyword(Keyword::Or),
                "not" => Tok::Keyword(Keyword::Not),
                "true" => Tok::Keyword(Keyword::True),
                "false" => Tok::Keyword(Keyword::False),
                "null" => Tok::Keyword(Keyword::Null),
                "in" => Tok::Keyword(Keyword::In),
                "between" => Tok::Keyword(Keyword::Between),
                "like" => Tok::Keyword(Keyword::Like),
                "is" => Tok::Keyword(Keyword::Is),
                "limit" => Tok::Keyword(Keyword::Limit),
                "order" => Tok::Keyword(Keyword::Order),
                "group" => Tok::Keyword(Keyword::Group),
                "as" => Tok::Keyword(Keyword::As),
                _ => Tok::Ident(word.to_string()),
            });
            continue;
        }
        return None;
    }
    Some(out)
}

fn is_literal_start(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Op(_) | Tok::LParen | Tok::Comma | Tok::Keyword(Keyword::And | Keyword::Or | Keyword::Where)
    )
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Tok]) -> Self {
        Self { toks, pos: 0 }
    }
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn bump(&mut self) -> Option<&Tok> {
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }
    fn expect_kw(&mut self, kw: Keyword) -> Option<()> {
        match self.peek()? {
            Tok::Keyword(k) if *k == kw => {
                self.pos += 1;
                Some(())
            }
            _ => None,
        }
    }

    /// Parse a whole statement. Returns `None` on anything that
    /// doesn't fit `SELECT … FROM tzip [WHERE …]
    /// [GROUP/ORDER/LIMIT …]` with the WHERE clause reduced to a
    /// pure AND of simple predicates. Trailing clauses after the
    /// WHERE are allowed (the predicates still cover every row
    /// DuckDB will inspect).
    fn parse_statement(&mut self) -> Option<Vec<Predicate>> {
        self.expect_kw(Keyword::Select)?;
        // Skip the projection — anything up to the matching FROM.
        // Rejects CTEs / set operations / nested selects by
        // refusing parens here.
        self.skip_until_from_at_top_level()?;
        self.expect_kw(Keyword::From)?;
        // FROM must be the single identifier `tzip` (the view
        // `query::run_duckdb` constructs). Anything else means
        // joins or extra sources we can't reason about.
        match self.bump()? {
            Tok::Ident(name) if name.eq_ignore_ascii_case("tzip") => {}
            _ => return None,
        }
        // Reject a comma or JOIN after the FROM table.
        if matches!(
            self.peek(),
            Some(Tok::Comma) | Some(Tok::Ident(_)) | Some(Tok::Keyword(Keyword::As))
        ) {
            // Alias via `AS alias` or whitespace-alias is fine
            // and doesn't add joins.
            if let Some(Tok::Keyword(Keyword::As)) = self.peek() {
                self.pos += 1;
                // Consume one identifier (the alias).
                if !matches!(self.bump(), Some(Tok::Ident(_))) {
                    return None;
                }
            } else if let Some(Tok::Ident(_)) = self.peek() {
                self.pos += 1;
            } else {
                return None;
            }
        }
        // Expect either WHERE … or an end-of-query marker.
        match self.peek() {
            Some(Tok::Keyword(Keyword::Where)) => {
                self.pos += 1;
                self.parse_and_chain()
            }
            None
            | Some(Tok::Semicolon)
            | Some(Tok::Keyword(Keyword::Order | Keyword::Group | Keyword::Limit)) => {
                // No WHERE → no predicates to prune on.
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Skip tokens until we hit a top-level `FROM`. Accepts
    /// balanced parens inside the projection (so `COUNT(*)`,
    /// `date_trunc('day', ts)`, etc. parse) but refuses a
    /// nested `SELECT` — a scalar subquery in the projection
    /// could reference columns we don't have stats on, and
    /// row-group pruning against an outer predicate would still
    /// be sound, but to stay narrow we keep the grammar
    /// conservative.
    fn skip_until_from_at_top_level(&mut self) -> Option<()> {
        let mut depth: usize = 0;
        while let Some(t) = self.peek() {
            match t {
                Tok::Keyword(Keyword::From) if depth == 0 => return Some(()),
                Tok::LParen => {
                    depth += 1;
                    self.pos += 1;
                }
                Tok::RParen => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                Tok::Keyword(Keyword::Select) => return None,
                _ => {
                    self.pos += 1;
                }
            }
        }
        None
    }

    fn parse_and_chain(&mut self) -> Option<Vec<Predicate>> {
        let mut preds = Vec::new();
        loop {
            let p = self.parse_simple_predicate()?;
            preds.push(p);
            match self.peek() {
                Some(Tok::Keyword(Keyword::And)) => {
                    self.pos += 1;
                    continue;
                }
                None
                | Some(Tok::Semicolon)
                | Some(Tok::Keyword(Keyword::Order | Keyword::Group | Keyword::Limit)) => {
                    return Some(preds)
                }
                _ => return None,
            }
        }
    }

    fn parse_simple_predicate(&mut self) -> Option<Predicate> {
        // column
        let column = match self.bump()? {
            Tok::Ident(s) => s.clone(),
            Tok::QuotedIdent(s) => s.clone(),
            _ => return None,
        };
        // op
        let op = match self.bump()? {
            Tok::Op(o) => *o,
            _ => return None,
        };
        // value
        let value = match self.bump()? {
            Tok::Int(n) => Literal::Int(*n),
            Tok::Float(f) => Literal::Float(*f),
            Tok::Str(s) => Literal::Str(s.clone()),
            Tok::Keyword(Keyword::True) => Literal::Bool(true),
            Tok::Keyword(Keyword::False) => Literal::Bool(false),
            _ => return None,
        };
        Some(Predicate { column, op, value })
    }
}

// ── Row-group stats pruning ────────────────────────────────────────────────

/// A row-group matches if NOTHING in the predicate list proves
/// it to be empty. Any individual "definitely empty" predicate
/// rules the row group out.
pub fn row_group_matches(
    rg: &parquet::file::metadata::RowGroupMetaData,
    preds: &[Predicate],
) -> bool {
    for p in preds {
        let Some(col) = rg.columns().iter().find(|c| {
            c.column_path().string().eq_ignore_ascii_case(&p.column)
                || c.column_descr().name().eq_ignore_ascii_case(&p.column)
        }) else {
            // Column not in schema → we can't reason; be safe
            // and keep.
            continue;
        };
        let Some(stats) = col.statistics() else {
            continue;
        };
        if definitely_excludes(stats, p.op, &p.value) {
            return false;
        }
    }
    true
}

fn definitely_excludes(stats: &Statistics, op: Op, lit: &Literal) -> bool {
    // Parquet's `Statistics::min_bytes_opt`/`max_bytes_opt` give
    // us the raw little-endian encoding of the typed stats.
    // Easier to pattern-match the enum variants.
    match (stats, lit) {
        (Statistics::Int32(s), Literal::Int(v)) => {
            let (min, max) = (s.min_opt().copied(), s.max_opt().copied());
            excludes_num(min, max, op, *v, |a| a as i64)
        }
        (Statistics::Int64(s), Literal::Int(v)) => {
            let (min, max) = (s.min_opt().copied(), s.max_opt().copied());
            excludes_num(min, max, op, *v, |a| a)
        }
        (Statistics::Float(s), Literal::Float(v)) => {
            let (min, max) = (s.min_opt().copied(), s.max_opt().copied());
            excludes_num_f(min, max, op, *v, |a| a as f64)
        }
        (Statistics::Double(s), Literal::Float(v)) => {
            let (min, max) = (s.min_opt().copied(), s.max_opt().copied());
            excludes_num_f(min, max, op, *v, |a| a)
        }
        (Statistics::Float(s), Literal::Int(v)) => {
            let (min, max) = (s.min_opt().copied(), s.max_opt().copied());
            excludes_num_f(min, max, op, *v as f64, |a| a as f64)
        }
        (Statistics::Double(s), Literal::Int(v)) => {
            let (min, max) = (s.min_opt().copied(), s.max_opt().copied());
            excludes_num_f(min, max, op, *v as f64, |a| a)
        }
        (Statistics::ByteArray(s), Literal::Str(v)) => {
            let min = s
                .min_opt()
                .and_then(|m| std::str::from_utf8(m.data()).ok());
            let max = s
                .max_opt()
                .and_then(|m| std::str::from_utf8(m.data()).ok());
            excludes_str(min, max, op, v)
        }
        (Statistics::Boolean(s), Literal::Bool(v)) => {
            let min = s.min_opt().copied();
            let max = s.max_opt().copied();
            // For a Boolean column, "excludes" reduces to
            // "every row in this group is the other value".
            match op {
                Op::Eq => matches!((min, max, *v),
                    (Some(false), Some(false), true) | (Some(true), Some(true), false)),
                Op::Ne => matches!((min, max, *v),
                    (Some(true), Some(true), true) | (Some(false), Some(false), false)),
                _ => false,
            }
        }
        _ => false,
    }
}

fn excludes_num<T, F>(min: Option<T>, max: Option<T>, op: Op, v: i64, to_i64: F) -> bool
where
    F: Fn(T) -> i64,
    T: Copy,
{
    let (Some(min), Some(max)) = (min, max) else {
        return false;
    };
    let (min, max) = (to_i64(min), to_i64(max));
    match op {
        Op::Eq => v < min || v > max,
        Op::Ne => min == max && min == v,
        Op::Lt => min >= v,
        Op::Le => min > v,
        Op::Gt => max <= v,
        Op::Ge => max < v,
    }
}

fn excludes_num_f<T, F>(min: Option<T>, max: Option<T>, op: Op, v: f64, to_f64: F) -> bool
where
    F: Fn(T) -> f64,
    T: Copy,
{
    let (Some(min), Some(max)) = (min, max) else {
        return false;
    };
    let (min, max) = (to_f64(min), to_f64(max));
    if min.is_nan() || max.is_nan() || v.is_nan() {
        return false;
    }
    match op {
        Op::Eq => v < min || v > max,
        Op::Ne => min == max && min == v,
        Op::Lt => min >= v,
        Op::Le => min > v,
        Op::Gt => max <= v,
        Op::Ge => max < v,
    }
}

fn excludes_str(min: Option<&str>, max: Option<&str>, op: Op, v: &str) -> bool {
    let (Some(min), Some(max)) = (min, max) else {
        return false;
    };
    match op {
        Op::Eq => v < min || v > max,
        Op::Ne => min == max && min == v,
        Op::Lt => min >= v,
        Op::Le => min > v,
        Op::Gt => max <= v,
        Op::Ge => max < v,
    }
}

// ── Pruned output assembly ─────────────────────────────────────────────────

/// Read `src` as a Parquet file, keep only row groups whose
/// indices appear in `keep`, and write the result to `out`.
/// Uses parquet-rs's streaming row-group reader + writer so the
/// peak live memory is one row group's worth of columns, not the
/// whole table.
pub fn compose_pruned<P: AsRef<Path>>(src: &[u8], keep: &[usize], out: P) -> Result<()> {
    use parquet::arrow::arrow_reader::{ArrowReaderBuilder, ParquetRecordBatchReaderBuilder};
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    let bytes = bytes::Bytes::copy_from_slice(src);
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes.clone())
        .context("open source parquet for pruning")?;
    let schema = builder.schema().clone();

    // Project all columns; filter by the row-group indices we
    // kept. `with_row_groups` tells parquet-rs to decode only the
    // given groups.
    let reader = builder
        .with_row_groups(keep.to_vec())
        .build()
        .context("build pruned parquet reader")?;

    let file = std::fs::File::create(out.as_ref())
        .with_context(|| format!("create pruned parquet temp file {}", out.as_ref().display()))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer =
        ArrowWriter::try_new(file, schema, Some(props)).context("open pruned parquet writer")?;
    for batch in reader {
        let batch = batch.context("decode row group during prune")?;
        writer.write(&batch).context("write pruned record batch")?;
    }
    writer.close().context("close pruned parquet writer")?;
    Ok(())
}

/// Convenience: given the sidecar body, predicates, and an
/// output path, run the full prune+compose pipeline. Returns:
/// - `Ok(Some(kept_count))` on success, where `kept_count` is
///   how many row groups survived.
/// - `Ok(None)` when every row group matches (caller can skip
///   the compose work and just write the full body out).
/// - `Err(_)` only on genuine Parquet errors.
pub fn prune_to_file<P: AsRef<Path>>(
    src: &[u8],
    preds: &[Predicate],
    out: P,
) -> Result<Option<usize>> {
    use parquet::arrow::arrow_reader::ArrowReaderMetadata;
    let bytes = bytes::Bytes::copy_from_slice(src);
    let meta = ArrowReaderMetadata::load(&bytes, Default::default())
        .context("load parquet metadata for prune")?;
    let total = meta.metadata().num_row_groups();
    if total == 0 {
        bail!("parquet sidecar has zero row groups");
    }
    let mut keep: Vec<usize> = Vec::with_capacity(total);
    for i in 0..total {
        if row_group_matches(meta.metadata().row_group(i), preds) {
            keep.push(i);
        }
    }
    if keep.len() == total {
        return Ok(None);
    }
    compose_pruned(src, &keep, out)?;
    Ok(Some(keep.len()))
}

// ── Range-reading a sidecar embedded in a ZIP ──────────────────────────────

/// `ChunkReader` for a byte range inside a tzip archive's
/// `Read + Seek` source. Lets parquet-rs read the Parquet
/// footer with one small range request, then fetch only the
/// column chunks for row groups we chose to keep — the whole
/// point of ChatGPT review #4.
///
/// The underlying source is shared via `Arc<Mutex<_>>` because
/// `ChunkReader::get_bytes` takes `&self` and parquet-rs may
/// invoke it in parallel; the mutex serialises the actual
/// seek+read pair against the source's own cursor.
pub struct ArchiveEntrySlice {
    source: Arc<Mutex<Box<dyn crate::sources::ReadSeek>>>,
    base: u64,
    len: u64,
}

impl ArchiveEntrySlice {
    pub fn new(
        source: Box<dyn crate::sources::ReadSeek>,
        base: u64,
        len: u64,
    ) -> Self {
        Self {
            source: Arc::new(Mutex::new(source)),
            base,
            len,
        }
    }
}

impl parquet::file::reader::Length for ArchiveEntrySlice {
    fn len(&self) -> u64 {
        self.len
    }
}

impl parquet::file::reader::ChunkReader for ArchiveEntrySlice {
    type T = io::Cursor<Vec<u8>>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        if start > self.len {
            return Err(parquet::errors::ParquetError::General(format!(
                "ArchiveEntrySlice::get_read start {} past len {}",
                start, self.len
            )));
        }
        let want = (self.len - start) as usize;
        let mut guard = self
            .source
            .lock()
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        guard
            .seek(SeekFrom::Start(self.base + start))
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        let mut buf = vec![0u8; want];
        guard
            .read_exact(&mut buf)
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        Ok(io::Cursor::new(buf))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        if start + length as u64 > self.len {
            return Err(parquet::errors::ParquetError::General(format!(
                "ArchiveEntrySlice::get_bytes [{}, +{}) past len {}",
                start, length, self.len
            )));
        }
        let mut guard = self
            .source
            .lock()
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        guard
            .seek(SeekFrom::Start(self.base + start))
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        let mut buf = vec![0u8; length];
        guard
            .read_exact(&mut buf)
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        Ok(Bytes::from(buf))
    }
}

/// Result of `prune_from_slice`. Also surfaces the total
/// row-group count so the caller can print the ratio in the
/// "N/M row groups kept" diagnostic even when the prune path
/// short-circuited on "nothing pruned".
pub struct PruneReport {
    /// `Some(kept)` when a pruned Parquet was written; `None`
    /// when every row group would survive and the caller
    /// should fall back to copying the whole body (same final
    /// bytes for DuckDB either way).
    pub pruned: Option<usize>,
    pub total: usize,
}

/// Prune straight off the archive's source, writing only the
/// kept row groups' decoded data to `out`. Reads only the
/// Parquet footer up front; then the parquet reader pulls the
/// column chunks for the kept row groups on demand. Full
/// bytes-saved for the remote case: no extra round-trip past
/// the footer + the data pages we actually want.
pub fn prune_from_slice<P: AsRef<Path>>(
    slice: ArchiveEntrySlice,
    preds: &[Predicate],
    out: P,
) -> Result<PruneReport> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    let builder = ParquetRecordBatchReaderBuilder::try_new(slice)
        .context("open parquet range reader for prune")?;
    let total = builder.metadata().num_row_groups();
    if total == 0 {
        bail!("parquet sidecar has zero row groups");
    }
    let mut keep: Vec<usize> = Vec::with_capacity(total);
    for i in 0..total {
        if row_group_matches(builder.metadata().row_group(i), preds) {
            keep.push(i);
        }
    }
    if keep.len() == total {
        return Ok(PruneReport {
            pruned: None,
            total,
        });
    }
    let schema = builder.schema().clone();
    let reader = builder
        .with_row_groups(keep.clone())
        .build()
        .context("build pruned range-read reader")?;
    let file = std::fs::File::create(out.as_ref())
        .with_context(|| format!("create pruned temp file {}", out.as_ref().display()))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer =
        ArrowWriter::try_new(file, schema, Some(props)).context("open pruned parquet writer")?;
    for batch in reader {
        let batch = batch.context("decode row group during range-read prune")?;
        writer.write(&batch).context("write pruned record batch")?;
    }
    writer.close().context("close pruned parquet writer")?;
    Ok(PruneReport {
        pruned: Some(keep.len()),
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(sql: &str) -> Option<Vec<Predicate>> {
        extract_conjunctive_predicates(sql)
    }

    #[test]
    fn parses_no_where() {
        assert_eq!(p("SELECT * FROM tzip").as_deref(), Some(&[][..]));
    }

    #[test]
    fn accepts_function_calls_in_projection() {
        // Common real-world projection forms: `COUNT(*)`,
        // `date_trunc('day', ts)` etc. — balanced parens inside
        // the projection are fine as long as there's no nested
        // SELECT.
        assert_eq!(
            p("SELECT COUNT(*) FROM tzip WHERE n > 100").as_deref(),
            Some(&[Predicate {
                column: "n".into(),
                op: Op::Gt,
                value: Literal::Int(100),
            }][..])
        );
        assert!(p("select date_trunc('day', ts) FROM tzip").is_some());
    }

    #[test]
    fn rejects_scalar_subquery_in_projection() {
        // A nested SELECT in the projection could reference
        // columns we don't have stats on; stay conservative.
        assert!(p("SELECT (SELECT 1) FROM tzip").is_none());
    }

    #[test]
    fn parses_single_predicate() {
        let got = p("SELECT * FROM tzip WHERE status = 200").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].column, "status");
        assert_eq!(got[0].op, Op::Eq);
        assert_eq!(got[0].value, Literal::Int(200));
    }

    #[test]
    fn parses_and_chain() {
        let got =
            p("SELECT * FROM tzip WHERE status = 200 AND latency_ms < 100.0 AND user = 'alice'")
                .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].value, Literal::Str("alice".into()));
    }

    #[test]
    fn parses_trailing_order_limit() {
        let got =
            p("SELECT * FROM tzip WHERE n > 10 ORDER BY ts LIMIT 100").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].op, Op::Gt);
    }

    #[test]
    fn rejects_or() {
        assert!(p("SELECT * FROM tzip WHERE a = 1 OR b = 2").is_none());
    }

    #[test]
    fn rejects_parens() {
        assert!(p("SELECT * FROM tzip WHERE (a = 1) AND b = 2").is_none());
    }

    #[test]
    fn rejects_not() {
        assert!(p("SELECT * FROM tzip WHERE NOT a = 1").is_none());
    }

    #[test]
    fn rejects_in_list() {
        assert!(p("SELECT * FROM tzip WHERE a IN (1, 2, 3)").is_none());
    }

    #[test]
    fn rejects_between() {
        assert!(p("SELECT * FROM tzip WHERE a BETWEEN 1 AND 5").is_none());
    }

    #[test]
    fn rejects_join() {
        assert!(p("SELECT * FROM tzip JOIN other ON tzip.a = other.a").is_none());
    }

    #[test]
    fn rejects_non_tzip_from() {
        assert!(p("SELECT * FROM something_else").is_none());
    }

    #[test]
    fn tokenizes_ops() {
        for op in ["=", "<", "<=", ">", ">=", "<>", "!="] {
            let sql = format!("SELECT * FROM tzip WHERE a {} 1", op);
            assert!(p(&sql).is_some(), "op {} should parse", op);
        }
    }

    #[test]
    fn excludes_int_eq() {
        let s = Statistics::int64(Some(10), Some(100), None, Some(0), false);
        assert!(definitely_excludes(&s, Op::Eq, &Literal::Int(5)));
        assert!(!definitely_excludes(&s, Op::Eq, &Literal::Int(50)));
        assert!(definitely_excludes(&s, Op::Eq, &Literal::Int(200)));
    }

    #[test]
    fn excludes_int_lt() {
        let s = Statistics::int64(Some(10), Some(100), None, Some(0), false);
        // col < 10 on [10,100] → every row ≥ 10, so none satisfy < 10.
        assert!(definitely_excludes(&s, Op::Lt, &Literal::Int(10)));
        // col < 11 on [10,100] → row with 10 satisfies; keep.
        assert!(!definitely_excludes(&s, Op::Lt, &Literal::Int(11)));
    }

    #[test]
    fn excludes_str_eq() {
        // Statistics for a ByteArray column.
        let s = Statistics::byte_array(
            Some(parquet::data_type::ByteArray::from("alice".as_bytes().to_vec())),
            Some(parquet::data_type::ByteArray::from("zed".as_bytes().to_vec())),
            None,
            Some(0),
            false,
        );
        assert!(definitely_excludes(&s, Op::Eq, &Literal::Str("abe".into())));
        assert!(!definitely_excludes(&s, Op::Eq, &Literal::Str("bob".into())));
    }

    #[test]
    fn missing_stats_keeps_row_group() {
        let s = Statistics::int64(None, None, None, None, false);
        assert!(!definitely_excludes(&s, Op::Eq, &Literal::Int(5)));
    }
}
