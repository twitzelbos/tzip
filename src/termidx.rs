//! Hashed inverted term index over text entries.
//!
//! Phase 2 of the Elastic feature-parity roadmap. At create time with
//! `tzip create ... --index-terms '*.json' --index-terms '*.log'`, every
//! matching entry gets tokenized + case-folded and the resulting term
//! set is MPH'd into a `.tzip/terms.v1` entry. At query time,
//! `tzip grep LITERAL archive.zip` consults the index to find the set
//! of entries that even *could* contain the literal; the regex scan
//! then runs only against those. For a 100-entry corpus where the
//! term appears in 3 entries, that's a 33× reduction in body bytes
//! fetched / decompressed / scanned.
//!
//! The index is **a filter, not a ranker**: tzip grep's output still
//! lists every matching line with context, we just dodge entries that
//! literally cannot contain the term.
//!
//! ## Wire format of `.tzip/terms.v1`
//!
//! ```text
//! +------------+-----------+-------------+----------------+
//! | magic (4)  | ver (2)   | n_terms (4) | mph_blob_len (4) |
//! +------------+-----------+-------------+----------------+
//! | mph_blob (bincode-serialised boomphf::Mphf<Vec<u8>>)  |
//! +-------------------------------------------------------+
//! | slots: for each MPH slot:                             |
//! |   term_len (u16) | term_bytes | posting_len (u32)     |
//! |   posting_list: u32[] sorted ascending entry indices  |
//! +-------------------------------------------------------+
//! ```
//!
//! Postings are plain `u32` (unsorted-sized vec of entry indices)
//! rather than varint-delta-compressed — simpler v1, bigger index.
//! A follow-up can tighten the posting encoding.
//!
//! ## Tokenizer
//!
//! ASCII case-folding, split on any non-ASCII-alnum byte. Tokens
//! shorter than 3 chars or longer than 64 are dropped — the short
//! ones are too common to filter usefully ("a", "is", "the"), the
//! long ones are usually hashes or noise. Non-ASCII byte sequences
//! pass through as-is (they still split on ASCII punctuation),
//! which gives reasonable behaviour on UTF-8 text without a full
//! Unicode tokenizer.

use anyhow::{anyhow, bail, Context, Result};
use boomphf::Mphf;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};

pub const INDEX_ENTRY_NAME: &str = ".tzip/terms.v1";
const MAGIC: [u8; 4] = *b"TZTI";
const VERSION: u16 = 1;

/// Tokenizer config. Tokens outside `[MIN_TOKEN_LEN, MAX_TOKEN_LEN]`
/// bytes are skipped. 3-char lower bound filters out "a", "is", "to"
/// etc. that would blow up index size for no filter value; 64-char
/// upper bound trims pathological inputs like 1024-char hash strings.
const MIN_TOKEN_LEN: usize = 3;
const MAX_TOKEN_LEN: usize = 64;

/// Tokenize `text`, appending each unique lowercase ASCII token
/// (that passes the length filter) to `out`. Bytes outside
/// `[a-zA-Z0-9]` are token boundaries. Non-ASCII bytes split too
/// (they're treated as part of a run if adjacent to alnum, otherwise
/// as delimiters — the result is "reasonable on UTF-8 text").
pub fn tokenize(text: &[u8], out: &mut HashSet<Vec<u8>>) {
    let mut cur: Vec<u8> = Vec::with_capacity(32);
    for &b in text {
        if b.is_ascii_alphanumeric() {
            cur.push(b.to_ascii_lowercase());
        } else if !cur.is_empty() {
            if cur.len() >= MIN_TOKEN_LEN && cur.len() <= MAX_TOKEN_LEN {
                if !out.contains(&cur) {
                    out.insert(cur.clone());
                }
            }
            cur.clear();
        }
    }
    if !cur.is_empty() && cur.len() >= MIN_TOKEN_LEN && cur.len() <= MAX_TOKEN_LEN {
        if !out.contains(&cur) {
            out.insert(cur);
        }
    }
}

/// Build-time accumulator. Feed each selected entry's body via
/// `add_entry(entry_idx, body)`, then call `finish()` to get the
/// serialized index blob.
pub struct TermIndexBuilder {
    /// term → sorted-set of entry indices that contain it
    postings: HashMap<Vec<u8>, Vec<u32>>,
}

impl TermIndexBuilder {
    pub fn new() -> Self {
        Self { postings: HashMap::new() }
    }

    pub fn add_entry(&mut self, entry_idx: u32, body: &[u8]) {
        let mut tokens: HashSet<Vec<u8>> = HashSet::new();
        tokenize(body, &mut tokens);
        self.add_prepared(entry_idx, tokens.into_iter().collect());
    }

    /// Add already-tokenized unique terms (produced by `tokenize` in
    /// the encode worker, then shipped to the main thread so posting
    /// indices match CDR-emit order).
    pub fn add_prepared(&mut self, entry_idx: u32, tokens: Vec<Vec<u8>>) {
        for t in tokens {
            // Deduplicate per-entry: a term can appear 1000× in one
            // body but should only contribute one posting entry.
            let list = self.postings.entry(t).or_default();
            if list.last() != Some(&entry_idx) {
                list.push(entry_idx);
            }
        }
    }

    /// Serialize as `.tzip/terms.v1` payload. Empty input OK — returns
    /// a valid empty-index payload that readers parse as
    /// zero-posting.
    pub fn finish(self) -> Result<Vec<u8>> {
        let terms: Vec<Vec<u8>> = self.postings.keys().cloned().collect();
        if terms.is_empty() {
            // Emit a valid "no-terms" index. Reader treats as
            // never-hits.
            let mut out: Vec<u8> = Vec::new();
            out.write_all(&MAGIC)?;
            out.write_u16::<LittleEndian>(VERSION)?;
            out.write_u32::<LittleEndian>(0)?;
            out.write_u32::<LittleEndian>(0)?;
            return Ok(out);
        }
        let mph = Mphf::new(1.7, &terms);
        // Layout: one slot per term, keyed by MPH slot index.
        let n = terms.len();
        let mut slots: Vec<(Vec<u8>, Vec<u32>)> = vec![(Vec::new(), Vec::new()); n];
        for term in &terms {
            let slot = mph
                .try_hash(term)
                .ok_or_else(|| anyhow!("MPH build dropped term"))? as usize;
            if slot >= n {
                bail!("MPH slot {} out of range (n={})", slot, n);
            }
            let mut postings = self.postings[term].clone();
            postings.sort_unstable();
            postings.dedup();
            slots[slot] = (term.clone(), postings);
        }
        let mph_blob = bincode::serialize(&mph).context("serialize MPH over terms")?;
        let mut out: Vec<u8> = Vec::with_capacity(16 + mph_blob.len() + n * 24);
        out.write_all(&MAGIC)?;
        out.write_u16::<LittleEndian>(VERSION)?;
        out.write_u32::<LittleEndian>(n as u32)?;
        out.write_u32::<LittleEndian>(mph_blob.len() as u32)?;
        out.write_all(&mph_blob)?;
        for (term, postings) in &slots {
            if term.len() > u16::MAX as usize {
                bail!("term too long: {} bytes", term.len());
            }
            out.write_u16::<LittleEndian>(term.len() as u16)?;
            out.write_all(term)?;
            out.write_u32::<LittleEndian>(postings.len() as u32)?;
            for p in postings {
                out.write_u32::<LittleEndian>(*p)?;
            }
        }
        Ok(out)
    }
}

/// Parsed read-side term index.
pub struct TermIndex {
    mph: Option<Mphf<Vec<u8>>>,
    /// Parallel to the MPH's slot space. Each slot holds
    /// `(canonical_term, posting_list)`. Empty when `mph` is `None`
    /// (zero-term archive).
    slots: Vec<(Vec<u8>, Vec<u32>)>,
}

impl TermIndex {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cur = Cursor::new(payload);
        let mut magic = [0u8; 4];
        cur.read_exact(&mut magic).context("read terms magic")?;
        if magic != MAGIC {
            bail!("term index: bad magic {:?}", magic);
        }
        let ver = cur.read_u16::<LittleEndian>()?;
        if ver != VERSION {
            bail!("term index: unsupported version {}", ver);
        }
        let n = cur.read_u32::<LittleEndian>()? as usize;
        let mph_len = cur.read_u32::<LittleEndian>()? as usize;
        if n == 0 {
            return Ok(Self { mph: None, slots: Vec::new() });
        }
        let pos = cur.position() as usize;
        if pos + mph_len > payload.len() {
            bail!("term index: mph_blob extends past payload");
        }
        let mph: Mphf<Vec<u8>> =
            bincode::deserialize(&payload[pos..pos + mph_len]).context("parse MPH blob")?;
        let mut cur = Cursor::new(&payload[pos + mph_len..]);
        let mut slots: Vec<(Vec<u8>, Vec<u32>)> = Vec::with_capacity(n);
        for _ in 0..n {
            let tl = cur.read_u16::<LittleEndian>()? as usize;
            let mut term = vec![0u8; tl];
            cur.read_exact(&mut term)?;
            let pl = cur.read_u32::<LittleEndian>()? as usize;
            let mut postings = Vec::with_capacity(pl);
            for _ in 0..pl {
                postings.push(cur.read_u32::<LittleEndian>()?);
            }
            slots.push((term, postings));
        }
        Ok(Self { mph: Some(mph), slots })
    }

    /// Postings for a known-canonical lowercase term. `None` if the
    /// term wasn't in the index.
    pub fn postings(&self, term: &[u8]) -> Option<&[u32]> {
        let mph = self.mph.as_ref()?;
        let slot = mph.try_hash(&term.to_vec())? as usize;
        if slot >= self.slots.len() {
            return None;
        }
        let (canon, posts) = &self.slots[slot];
        if canon == term {
            Some(posts)
        } else {
            None
        }
    }

    /// Resolve a grep query to the set of entry indices that could
    /// contain it, by tokenizing the query and intersecting the
    /// posting lists of every token.
    ///
    /// Returns `None` when the query has no usable tokens (all too
    /// short, pure punctuation) — caller should skip the index and
    /// fall through to linear scan. Returns `Some(empty)` for a
    /// query whose tokens have **no** entries in the archive — caller
    /// can safely skip the grep entirely.
    pub fn candidates(&self, query: &str) -> Option<HashSet<u32>> {
        let mut tokens: HashSet<Vec<u8>> = HashSet::new();
        tokenize(query.as_bytes(), &mut tokens);
        if tokens.is_empty() {
            return None;
        }
        let mut sets: Vec<HashSet<u32>> = Vec::with_capacity(tokens.len());
        for t in &tokens {
            let posts = self.postings(t)?;
            sets.push(posts.iter().copied().collect());
        }
        // Start from the smallest set; intersect the rest.
        sets.sort_by_key(|s| s.len());
        let mut acc = sets.remove(0);
        for s in &sets {
            acc.retain(|k| s.contains(k));
            if acc.is_empty() {
                break;
            }
        }
        Some(acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_on_punctuation() {
        let mut out = HashSet::new();
        tokenize(b"Hello, World! Error: OutOfMemoryError at line 42", &mut out);
        let got: HashSet<&str> = out
            .iter()
            .map(|v| std::str::from_utf8(v).unwrap())
            .collect();
        assert!(got.contains("hello"));
        assert!(got.contains("world"));
        assert!(got.contains("error"));
        assert!(got.contains("outofmemoryerror"));
        assert!(got.contains("line"));
        // "42" is 2 chars, filtered.
        assert!(!got.contains("42"));
        // "at" is 2 chars, filtered.
        assert!(!got.contains("at"));
    }

    #[test]
    fn roundtrip_build_parse_query() {
        let mut b = TermIndexBuilder::new();
        b.add_entry(0, b"one two three four");
        b.add_entry(1, b"two three");
        b.add_entry(2, b"three four five");
        b.add_entry(3, b"six seven eight");
        let blob = b.finish().unwrap();
        let idx = TermIndex::parse(&blob).unwrap();

        // "three" appears in 0, 1, 2
        let c = idx.candidates("three").unwrap();
        assert_eq!(c, [0, 1, 2].into_iter().collect::<HashSet<_>>());

        // "two three" intersects: {0,1,2} ∩ {0,1} = {0,1}
        let c = idx.candidates("two three").unwrap();
        assert_eq!(c, [0, 1].into_iter().collect::<HashSet<_>>());

        // "nine" doesn't exist → None (unknown-term short-circuit)
        assert!(idx.candidates("nine").is_none());

        // Query with only sub-min-length tokens → None
        assert!(idx.candidates("a b c").is_none());
    }

    #[test]
    fn case_folding_works() {
        let mut b = TermIndexBuilder::new();
        b.add_entry(0, b"OutOfMemoryError happened");
        let blob = b.finish().unwrap();
        let idx = TermIndex::parse(&blob).unwrap();
        assert!(idx.candidates("outofmemoryerror").is_some());
        assert!(idx.candidates("OutOfMemoryError").is_some());
        assert!(idx.candidates("OUTOFMEMORYERROR").is_some());
    }

    #[test]
    fn empty_builder_produces_parsable_index() {
        let b = TermIndexBuilder::new();
        let blob = b.finish().unwrap();
        let idx = TermIndex::parse(&blob).unwrap();
        assert!(idx.postings(b"anything").is_none());
        assert!(idx.candidates("anything").is_none());
    }
}
