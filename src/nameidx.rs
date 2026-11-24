//! Minimal-perfect-hash index over CDR entry names.
//!
//! An optional `.tzip/names.v1` entry inside a tzip archive turns
//! `tzip cat NAME` / `tzip x NAME` / entry-lookup paths from `O(N)`
//! linear scans of the CDR into `O(1)` hash probes. Archives without
//! this entry fall back to the existing linear scan — the index is
//! strictly additive.
//!
//! ## Wire format of `.tzip/names.v1`
//!
//! ```text
//! +----+----+--------+-----------------------+---------------------+
//! | magic (4)        | version (2) | count (4) | mph_blob_len (4)  |
//! +------------------+-------------+-----------+-------------------+
//! | mph_blob (bincode-serialised boomphf::Mphf<&[u8]>)             |
//! +------------------+-------------+-----------+-------------------+
//! | name_count * [name_len u16 | name bytes | entry_idx u32]       |
//! +----------------------------------------------------------------+
//! ```
//!
//! The MPH maps every name to a unique index in `0..count`; the
//! verification table lets us reject a lookup for a name that **wasn't
//! in the original key set** (MPH returns a bogus-but-valid-looking
//! slot for inputs it's never seen). After the probe, we cross-check
//! the name against the stored canonical copy at that slot.
//!
//! The entry resolves to `entry_idx` into `Archive::entries()`.
//!
//! ## Why hashed and not sorted
//!
//! We chose MPH + verification over a sorted CDR-name table for three
//! reasons:
//!
//! - **O(1) lookup**, not O(log N). On a million-entry archive the
//!   difference is ~20 string compares per lookup.
//! - **Space**: MPH tables are typically ~3 bits per key. On a
//!   1M-entry archive that's ~400 KB of MPH + name-table — small vs
//!   the archive itself.
//! - **Build speed**: `boomphf` builds at ~3M keys/s, so even
//!   ten-million-entry archives build in seconds.
//!
//! Appendable archives (`tzip u`) need the index rebuilt — same cost
//! as creating it the first time. Phase 4 of the roadmap folds this
//! into a segmented layout so append doesn't force a full rebuild.

use anyhow::{anyhow, bail, Context, Result};
use boomphf::Mphf;
use std::io::{Cursor, Read, Write};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

/// Reserved entry name. Lands as a normal CDR entry so other tools
/// can see it (and ignore it); tzip picks it up at open time.
pub const INDEX_ENTRY_NAME: &str = ".tzip/names.v1";
const MAGIC: [u8; 4] = *b"TZNI";
const VERSION: u16 = 1;

/// Build side: returns the serialized `.tzip/names.v1` payload for an
/// archive whose entries have the given names (in CDR order, so index
/// `i` into the input slice is the `entry_idx` emitted for that name).
pub fn build(names: &[&str]) -> Result<Vec<u8>> {
    if names.is_empty() {
        bail!("name index must have at least one entry");
    }
    // Build the MPH over the raw name bytes. `gamma = 1.7` is boomphf's
    // documented sweet spot: ~1.0× overhead on build time for ~2% less
    // table size vs 2.0.
    let keys: Vec<Vec<u8>> = names.iter().map(|n| n.as_bytes().to_vec()).collect();
    let mph = Mphf::new(1.7, &keys);

    // Verification table: for each name, find its MPH slot and store
    // the canonical name at that slot. Lookup later compares the probed
    // name against this table.
    let mut slots: Vec<(Vec<u8>, u32)> = Vec::with_capacity(names.len());
    slots.resize(names.len(), (Vec::new(), 0));
    for (idx, k) in keys.iter().enumerate() {
        let slot = mph
            .try_hash(k)
            .ok_or_else(|| anyhow!("MPH build dropped key {:?}", String::from_utf8_lossy(k)))?
            as usize;
        if slot >= slots.len() {
            bail!("MPH slot {} out of range (len {})", slot, slots.len());
        }
        slots[slot] = (k.clone(), idx as u32);
    }

    // Serialize.
    let mph_blob = bincode::serialize(&mph).context("serialize MPH")?;
    let mut out: Vec<u8> = Vec::with_capacity(4 + 2 + 4 + 4 + mph_blob.len() + names.len() * 32);
    out.write_all(&MAGIC)?;
    out.write_u16::<LittleEndian>(VERSION)?;
    out.write_u32::<LittleEndian>(names.len() as u32)?;
    out.write_u32::<LittleEndian>(mph_blob.len() as u32)?;
    out.write_all(&mph_blob)?;
    for (name, idx) in &slots {
        if name.len() > u16::MAX as usize {
            bail!("entry name too long for index: {} bytes", name.len());
        }
        out.write_u16::<LittleEndian>(name.len() as u16)?;
        out.write_all(name)?;
        out.write_u32::<LittleEndian>(*idx)?;
    }
    Ok(out)
}

/// Read side: parsed `.tzip/names.v1` payload, ready for `lookup`.
pub struct NameIndex {
    mph: Mphf<Vec<u8>>,
    /// Canonical name + entry index at each MPH slot. Length = `count`.
    slots: Vec<(Vec<u8>, u32)>,
}

impl NameIndex {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cur = Cursor::new(payload);
        let mut magic = [0u8; 4];
        cur.read_exact(&mut magic).context("read index magic")?;
        if magic != MAGIC {
            bail!("name index: bad magic {:?}", magic);
        }
        let version = cur.read_u16::<LittleEndian>().context("read index version")?;
        if version != VERSION {
            bail!("name index: unsupported version {}", version);
        }
        let count = cur.read_u32::<LittleEndian>().context("read index count")? as usize;
        let mph_len = cur.read_u32::<LittleEndian>().context("read mph_blob_len")? as usize;
        let pos = cur.position() as usize;
        if pos + mph_len > payload.len() {
            bail!("name index: mph_blob extends past payload");
        }
        let mph: Mphf<Vec<u8>> =
            bincode::deserialize(&payload[pos..pos + mph_len]).context("parse MPH blob")?;
        let mut cur = Cursor::new(&payload[pos + mph_len..]);
        let mut slots: Vec<(Vec<u8>, u32)> = Vec::with_capacity(count);
        for _ in 0..count {
            let nl = cur.read_u16::<LittleEndian>().context("read name len")? as usize;
            let mut name_buf = vec![0u8; nl];
            cur.read_exact(&mut name_buf).context("read name bytes")?;
            let idx = cur.read_u32::<LittleEndian>().context("read entry idx")?;
            slots.push((name_buf, idx));
        }
        Ok(Self { mph, slots })
    }

    /// `Some(entry_idx)` if `name` was in the original key set.
    /// `None` for any other name — even one the MPH happens to map to a
    /// valid-looking slot, because we compare against the canonical
    /// name stored there.
    pub fn lookup(&self, name: &str) -> Option<u32> {
        let k = name.as_bytes();
        let slot = self.mph.try_hash(&k.to_vec())? as usize;
        if slot >= self.slots.len() {
            return None;
        }
        let (stored, idx) = &self.slots[slot];
        if stored.as_slice() == k {
            Some(*idx)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_smoke() {
        let names: Vec<&str> = (0..1000)
            .map(|i| {
                Box::leak(Box::new(format!("dir_{}/entry_{:04}.txt", i % 10, i)))
                    .as_str()
            })
            .collect();
        let payload = build(&names).unwrap();
        let idx = NameIndex::parse(&payload).unwrap();

        // Every known name resolves back to its original index.
        for (i, n) in names.iter().enumerate() {
            assert_eq!(idx.lookup(n), Some(i as u32), "name {}", n);
        }
        // Names not in the set resolve to None.
        assert_eq!(idx.lookup("does/not/exist.txt"), None);
        assert_eq!(idx.lookup(""), None);
        assert_eq!(idx.lookup("dir_0/entry_0000.txt.extra"), None);
    }

    #[test]
    fn unicode_names_work() {
        let names = vec!["αβγ.txt", "emoji_🎉.bin", "普通话.md", "ascii.ok"];
        let payload = build(&names).unwrap();
        let idx = NameIndex::parse(&payload).unwrap();
        for (i, n) in names.iter().enumerate() {
            assert_eq!(idx.lookup(n), Some(i as u32));
        }
    }

    #[test]
    fn single_entry_archive() {
        let names = vec!["one.txt"];
        let payload = build(&names).unwrap();
        let idx = NameIndex::parse(&payload).unwrap();
        assert_eq!(idx.lookup("one.txt"), Some(0));
        assert_eq!(idx.lookup("two.txt"), None);
    }
}
