//! Platform-agnostic VFS: builds a directory tree from an archive's CDR
//! and serves `read` / `getattr` / `lookup` / `readdir` calls backed by
//! an LRU cache of decompressed entry bodies.

use anyhow::Result;
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use crate::reader::stream_entry_to;
use crate::zipreader::Archive;

/// FUSE / FSKit inode. Inode 1 is the root directory by convention.
pub type Inode = u64;

pub const ROOT_INO: Inode = 1;

/// Canonical metadata view for an inode. Platform adapters translate
/// this into their native attribute struct.
#[derive(Clone, Copy)]
pub struct Attr {
    pub ino: Inode,
    pub size: u64,
    pub kind: NodeKind,
    pub mtime_unix: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

enum Node {
    Dir { mtime_unix: i64 },
    File { entry_idx: usize, size: u64, mtime_unix: i64 },
}

pub struct VirtualFs {
    /// The backing archive. `Mutex` because `source_mut` seeks.
    archive: Mutex<Archive>,
    /// Inode N lives at `nodes[N - 1]`. Root is `nodes[0]`.
    nodes: Vec<Node>,
    /// `parent_ino → ordered list of (name, child_ino, kind)`.
    children: HashMap<Inode, Vec<(Vec<u8>, Inode, NodeKind)>>,
    /// AES password, if the mounted archive has encrypted entries.
    password: Option<String>,
    /// LRU of decompressed entry bodies. See `EntryCache`.
    cache: Mutex<EntryCache>,
    /// Log cache diagnostics when enabled.
    verbose: bool,
}

struct EntryCache {
    lru: LruCache<Inode, Arc<Vec<u8>>>,
    bytes_in_use: usize,
    byte_budget: usize,
}

impl EntryCache {
    fn new(budget: usize) -> Self {
        // We bound by bytes, but `lru` needs a positive count cap too.
        // Any large number works; byte eviction is what actually keeps
        // us in check.
        Self {
            lru: LruCache::new(NonZeroUsize::new(1 << 20).unwrap()),
            bytes_in_use: 0,
            byte_budget: budget,
        }
    }

    fn get(&mut self, ino: Inode) -> Option<Arc<Vec<u8>>> {
        self.lru.get(&ino).cloned()
    }

    fn insert(&mut self, ino: Inode, body: Arc<Vec<u8>>) {
        let size = body.len();
        // Evict oldest entries until we fit, or until the cache is empty.
        while self.bytes_in_use.saturating_add(size) > self.byte_budget && !self.lru.is_empty() {
            if let Some((_, evicted)) = self.lru.pop_lru() {
                self.bytes_in_use = self.bytes_in_use.saturating_sub(evicted.len());
            } else {
                break;
            }
        }
        // Even if this one entry is larger than the whole budget, keep
        // it — re-decompressing per FUSE read (default 128 KiB) is
        // orders of magnitude worse than briefly exceeding the budget.
        self.bytes_in_use = self.bytes_in_use.saturating_add(size);
        if let Some(prev) = self.lru.put(ino, body) {
            self.bytes_in_use = self.bytes_in_use.saturating_sub(prev.len());
        }
    }
}

impl VirtualFs {
    pub fn new(
        archive: Archive,
        password: Option<String>,
        cache_mb: usize,
        verbose: bool,
    ) -> Result<Self> {
        let (nodes, children) = build_tree(&archive);
        Ok(Self {
            archive: Mutex::new(archive),
            nodes,
            children,
            password,
            cache: Mutex::new(EntryCache::new(cache_mb * 1024 * 1024)),
            verbose,
        })
    }

    pub fn getattr(&self, ino: Inode) -> Option<Attr> {
        self.nodes.get((ino - 1) as usize).map(|n| node_attr(ino, n))
    }

    pub fn lookup(&self, parent: Inode, name: &[u8]) -> Option<Attr> {
        let siblings = self.children.get(&parent)?;
        for (n, ino, _) in siblings {
            if n.as_slice() == name {
                return self.getattr(*ino);
            }
        }
        None
    }

    /// Returns `(ino, kind, name)` tuples for every child of `parent`,
    /// in CDR insertion order.
    pub fn readdir(&self, parent: Inode) -> Option<Vec<(Inode, NodeKind, Vec<u8>)>> {
        let siblings = self.children.get(&parent)?;
        Some(
            siblings
                .iter()
                .map(|(name, ino, kind)| (*ino, *kind, name.clone()))
                .collect(),
        )
    }

    /// Read `size` bytes at `offset` from the file-inode `ino`. Caches
    /// the decompressed body on first read.
    pub fn read(&self, ino: Inode, offset: u64, size: u32) -> Result<Vec<u8>> {
        let node = self
            .nodes
            .get((ino - 1) as usize)
            .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
        let entry_idx = match node {
            Node::File { entry_idx, .. } => *entry_idx,
            Node::Dir { .. } => anyhow::bail!("ino {} is a directory", ino),
        };

        // Cache hit?
        let body = {
            let mut cache = self.cache.lock().unwrap();
            cache.get(ino)
        };
        let body = match body {
            Some(b) => b,
            None => {
                let decoded = self.decompress_entry(entry_idx)?;
                let arc = Arc::new(decoded);
                {
                    let mut cache = self.cache.lock().unwrap();
                    cache.insert(ino, Arc::clone(&arc));
                }
                arc
            }
        };

        if offset >= body.len() as u64 {
            return Ok(Vec::new());
        }
        let end = ((offset as usize) + size as usize).min(body.len());
        Ok(body[offset as usize..end].to_vec())
    }

    fn decompress_entry(&self, entry_idx: usize) -> Result<Vec<u8>> {
        let mut arch = self.archive.lock().unwrap();
        let name = arch.entries()[entry_idx].name.clone();
        arch.lfh_total_size_of(entry_idx)?;
        let re = arch.entries()[entry_idx].clone();
        let expected = re.uncomp_size as usize;
        let mut out: Vec<u8> = Vec::with_capacity(expected.min(16 * 1024 * 1024));
        stream_entry_to(arch.source_mut(), &re, self.password.as_deref(), &mut out)?;
        if self.verbose && (expected > 16 * 1024 * 1024) {
            eprintln!(
                "tzip mount: decompressed {} ({} MiB) into cache",
                name,
                expected / (1024 * 1024)
            );
        }
        Ok(out)
    }
}

fn node_attr(ino: Inode, n: &Node) -> Attr {
    match n {
        Node::Dir { mtime_unix } => Attr {
            ino,
            size: 0,
            kind: NodeKind::Dir,
            mtime_unix: *mtime_unix,
        },
        Node::File { size, mtime_unix, .. } => Attr {
            ino,
            size: *size,
            kind: NodeKind::File,
            mtime_unix: *mtime_unix,
        },
    }
}

/// Builds the inode + children maps from the archive's CDR.
/// Synthetic directories are created for every intermediate path
/// component. Entries whose name ends in `/` are treated as explicit
/// directory markers.
fn build_tree(archive: &Archive) -> (Vec<Node>, HashMap<Inode, Vec<(Vec<u8>, Inode, NodeKind)>>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut children: HashMap<Inode, Vec<(Vec<u8>, Inode, NodeKind)>> = HashMap::new();
    // Root inode = 1 (nodes[0]).
    nodes.push(Node::Dir { mtime_unix: 0 });
    children.insert(ROOT_INO, Vec::new());

    // name-of-dir-at-parent → that dir's inode.
    let mut dir_by_path: HashMap<Vec<u8>, Inode> = HashMap::new();

    for (idx, entry) in archive.entries().iter().enumerate() {
        let full = entry.name.trim_start_matches('/');
        let is_dir_marker = full.ends_with('/');
        let path = full.trim_end_matches('/');
        if path.is_empty() {
            continue;
        }
        let parts: Vec<&str> = path.split('/').collect();
        let mut parent_ino = ROOT_INO;
        let mut running = String::new();
        for (j, part) in parts.iter().enumerate() {
            let is_last = j + 1 == parts.len();
            running.push_str(part);
            let dir_key = running.clone().into_bytes();
            if is_last {
                if is_dir_marker {
                    // Dir marker entry — register as a dir if new.
                    if dir_by_path.get(&dir_key).is_none() {
                        let ino = nodes.len() as Inode + 1;
                        nodes.push(Node::Dir {
                            mtime_unix: dos_to_unix(entry.mtime),
                        });
                        children.insert(ino, Vec::new());
                        children
                            .get_mut(&parent_ino)
                            .unwrap()
                            .push((part.as_bytes().to_vec(), ino, NodeKind::Dir));
                        dir_by_path.insert(dir_key, ino);
                    }
                } else {
                    // File entry.
                    let ino = nodes.len() as Inode + 1;
                    nodes.push(Node::File {
                        entry_idx: idx,
                        size: entry.uncomp_size,
                        mtime_unix: dos_to_unix(entry.mtime),
                    });
                    children
                        .get_mut(&parent_ino)
                        .unwrap()
                        .push((part.as_bytes().to_vec(), ino, NodeKind::File));
                }
            } else {
                // Intermediate dir — find or create.
                let ino = if let Some(&existing) = dir_by_path.get(&dir_key) {
                    existing
                } else {
                    let ino = nodes.len() as Inode + 1;
                    nodes.push(Node::Dir { mtime_unix: 0 });
                    children.insert(ino, Vec::new());
                    children
                        .get_mut(&parent_ino)
                        .unwrap()
                        .push((part.as_bytes().to_vec(), ino, NodeKind::Dir));
                    dir_by_path.insert(dir_key, ino);
                    ino
                };
                parent_ino = ino;
            }
            running.push('/');
        }
    }

    (nodes, children)
}

fn dos_to_unix((date, time): (u16, u16)) -> i64 {
    let year = 1980 + ((date >> 9) & 0x7F) as i64;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as i64;
    let minute = ((time >> 5) & 0x3F) as i64;
    let sec = ((time & 0x1F) << 1) as i64;
    let month = month.max(1);
    let day = day.max(1);
    civil_to_unix(year, month, day) + hour * 3600 + minute * 60 + sec
}

fn civil_to_unix(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    days * 86_400
}
