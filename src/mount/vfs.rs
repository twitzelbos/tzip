//! Platform-agnostic VFS: builds a directory tree from an archive's CDR
//! and serves `read` / `getattr` / `lookup` / `readdir` calls backed by
//! an LRU cache of decompressed entry bodies.
//!
//! When mounted writable (`tzip mount --write`), it also serves
//! `create` / `write` / `unlink` / `mkdir` / `rmdir` / `truncate` by
//! journaling changes in memory. The on-disk archive is untouched
//! until `commit()` runs (typically from the FUSE unmount path): at
//! that point the dirty bodies are materialised to a staging tempdir
//! and the whole thing lands through `pipeline::run` with
//! `update + atomic` semantics — so a crash before commit loses the
//! in-memory changes but never corrupts the archive.

use anyhow::{Context, Result};
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use crate::method::Method;
use crate::options::{Options, UserSetFlags};
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
    /// User-set mode bits via chmod, if any. `None` = platform adapter
    /// should fall through to its default (0o444/0o555 read-only,
    /// 0o644/0o755 writable).
    pub mode: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

enum Node {
    Dir {
        mtime_unix: i64,
        /// Mount-relative directory path (`""` for root). Used at
        /// commit time to re-emit new directories.
        #[allow(dead_code)]
        path: String,
        /// User-set mode bits via chmod. None = fall through to the
        /// writable-default 0o755.
        mode: Option<u32>,
    },
    File {
        source: FileSource,
        size: u64,
        mtime_unix: i64,
        /// Archive-relative entry name (e.g. `docs/README.md`). Used
        /// at commit time to materialise dirty bodies at the right
        /// path inside the staging dir, and to tombstone removed
        /// archive entries.
        path: String,
        /// User-set mode bits via chmod. None = fall through to the
        /// writable-default 0o644.
        mode: Option<u32>,
    },
}

impl Node {
    fn path(&self) -> &str {
        match self {
            Node::Dir { path, .. } | Node::File { path, .. } => path,
        }
    }
    fn mode(&self) -> Option<u32> {
        match self {
            Node::Dir { mode, .. } | Node::File { mode, .. } => *mode,
        }
    }
    fn set_path(&mut self, new_path: String) {
        match self {
            Node::Dir { path, .. } | Node::File { path, .. } => *path = new_path,
        }
    }
    fn set_mtime(&mut self, ts: i64) {
        match self {
            Node::Dir { mtime_unix, .. } | Node::File { mtime_unix, .. } => *mtime_unix = ts,
        }
    }
    fn set_mode_bits(&mut self, m: u32) {
        match self {
            Node::Dir { mode, .. } | Node::File { mode, .. } => *mode = Some(m),
        }
    }
}

#[derive(Clone, Copy)]
enum FileSource {
    /// Original archive entry at `entry_idx`.
    Archive(usize),
    /// Body lives in `MutableState::dirty`. Either a newly-created
    /// file or a previously archive-backed file that's been
    /// modified (`cow_in` moves it over).
    Dirty,
}

pub struct VirtualFs {
    /// The backing archive. `Mutex` because `source_mut` seeks.
    archive: Mutex<Archive>,
    /// The entire mutable VFS view — nodes, children, dirty, tombstones.
    /// RwLock because reads dominate even in write mode.
    state: RwLock<MutableState>,
    /// AES password, if the mounted archive has encrypted entries.
    password: Option<String>,
    /// LRU of decompressed entry bodies. Never caches dirty bodies —
    /// those live in `state.dirty`.
    cache: Mutex<EntryCache>,
    /// Log cache diagnostics when enabled.
    verbose: bool,
    /// Writable mount? Toggles all the mutating op gates.
    writable: bool,
    /// Absolute path to the backing archive on disk. Used by `commit`
    /// to drive `pipeline::run` in `update` mode.
    archive_path: PathBuf,
}

struct MutableState {
    /// Inode N lives at `nodes[N - 1]`. Root is `nodes[0]`.
    nodes: Vec<Node>,
    /// `parent_ino → ordered list of (name, child_ino, kind)`.
    children: HashMap<Inode, Vec<(Vec<u8>, Inode, NodeKind)>>,
    /// Pending-write bodies. Keyed by inode. On commit these land as
    /// archive entries at their corresponding Node::File::path.
    dirty: HashMap<Inode, Vec<u8>>,
    /// Original archive entry names to delete on commit. Only populated
    /// for archive-backed entries that were unlinked or replaced.
    tombstones: HashSet<String>,
}

struct EntryCache {
    lru: LruCache<Inode, Arc<Vec<u8>>>,
    bytes_in_use: usize,
    byte_budget: usize,
}

impl EntryCache {
    fn new(budget: usize) -> Self {
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
        while self.bytes_in_use.saturating_add(size) > self.byte_budget && !self.lru.is_empty() {
            if let Some((_, evicted)) = self.lru.pop_lru() {
                self.bytes_in_use = self.bytes_in_use.saturating_sub(evicted.len());
            } else {
                break;
            }
        }
        self.bytes_in_use = self.bytes_in_use.saturating_add(size);
        if let Some(prev) = self.lru.put(ino, body) {
            self.bytes_in_use = self.bytes_in_use.saturating_sub(prev.len());
        }
    }

    fn invalidate(&mut self, ino: Inode) {
        if let Some(prev) = self.lru.pop(&ino) {
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
        Self::new_full(archive, password, cache_mb, verbose, false, PathBuf::new())
    }

    /// Full constructor. When `writable` is true, `archive_path` must
    /// point at the on-disk archive so `commit()` can drive the update
    /// engine against it.
    pub fn new_full(
        archive: Archive,
        password: Option<String>,
        cache_mb: usize,
        verbose: bool,
        writable: bool,
        archive_path: PathBuf,
    ) -> Result<Self> {
        let (nodes, children) = build_tree(&archive);
        Ok(Self {
            archive: Mutex::new(archive),
            state: RwLock::new(MutableState {
                nodes,
                children,
                dirty: HashMap::new(),
                tombstones: HashSet::new(),
            }),
            password,
            cache: Mutex::new(EntryCache::new(cache_mb * 1024 * 1024)),
            verbose,
            writable,
            archive_path,
        })
    }

    pub fn getattr(&self, ino: Inode) -> Option<Attr> {
        let st = self.state.read().unwrap();
        st.nodes.get((ino - 1) as usize).map(|n| node_attr(ino, n))
    }

    pub fn lookup(&self, parent: Inode, name: &[u8]) -> Option<Attr> {
        let st = self.state.read().unwrap();
        let siblings = st.children.get(&parent)?;
        for (n, ino, _) in siblings {
            if n.as_slice() == name {
                return st.nodes.get((*ino - 1) as usize).map(|n| node_attr(*ino, n));
            }
        }
        None
    }

    /// Returns `(ino, kind, name)` tuples for every child of `parent`,
    /// in CDR insertion order.
    pub fn readdir(&self, parent: Inode) -> Option<Vec<(Inode, NodeKind, Vec<u8>)>> {
        let st = self.state.read().unwrap();
        let siblings = st.children.get(&parent)?;
        Some(
            siblings
                .iter()
                .map(|(name, ino, kind)| (*ino, *kind, name.clone()))
                .collect(),
        )
    }

    /// Read `size` bytes at `offset` from the file-inode `ino`. Dirty
    /// bodies return from the in-memory journal; archive-backed bodies
    /// stream through the usual LRU cache.
    pub fn read(&self, ino: Inode, offset: u64, size: u32) -> Result<Vec<u8>> {
        // Dirty short-path: serve from memory, no cache.
        {
            let st = self.state.read().unwrap();
            let node = st
                .nodes
                .get((ino - 1) as usize)
                .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
            if let Node::File { source: FileSource::Dirty, .. } = node {
                let body = st
                    .dirty
                    .get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("dirty ino {} has no body", ino))?;
                if offset >= body.len() as u64 {
                    return Ok(Vec::new());
                }
                let end = ((offset as usize) + size as usize).min(body.len());
                return Ok(body[offset as usize..end].to_vec());
            }
        }

        // Archive-backed path (original behaviour).
        let entry_idx = {
            let st = self.state.read().unwrap();
            let node = st
                .nodes
                .get((ino - 1) as usize)
                .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
            match node {
                Node::File {
                    source: FileSource::Archive(idx),
                    ..
                } => *idx,
                Node::Dir { .. } => anyhow::bail!("ino {} is a directory", ino),
                Node::File { source: FileSource::Dirty, .. } => unreachable!("handled above"),
            }
        };

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
        arch.lfh_total_size_of(entry_idx)?;
        let (name, expected) = {
            let re = arch.entry(entry_idx);
            (re.name.clone(), re.uncomp_size as usize)
        };
        let mut out: Vec<u8> = Vec::with_capacity(expected.min(16 * 1024 * 1024));
        arch.stream_entry_to(entry_idx, self.password.as_deref(), &mut out)?;
        if self.verbose && (expected > 16 * 1024 * 1024) {
            eprintln!(
                "tzip mount: decompressed {} ({} MiB) into cache",
                name,
                expected / (1024 * 1024)
            );
        }
        Ok(out)
    }

    // ── Write-side operations (writable mounts only) ──────────────────────

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Create a new regular file as a child of `parent`. Fails with
    /// `EEXIST` if a sibling with that name already exists.
    pub fn create(&self, parent: Inode, name: &[u8]) -> Result<Attr> {
        self.require_writable()?;
        let mut st = self.state.write().unwrap();
        self.create_locked(&mut st, parent, name)
    }

    fn create_locked(
        &self,
        st: &mut MutableState,
        parent: Inode,
        name: &[u8],
    ) -> Result<Attr> {
        let parent_node = st
            .nodes
            .get((parent - 1) as usize)
            .ok_or_else(|| anyhow::anyhow!("no such parent inode {}", parent))?;
        let parent_path = match parent_node {
            Node::Dir { path, .. } => path.clone(),
            Node::File { .. } => anyhow::bail!("parent ino {} is not a directory", parent),
        };
        let siblings = st.children.get(&parent).cloned().unwrap_or_default();
        if siblings.iter().any(|(n, _, _)| n.as_slice() == name) {
            anyhow::bail!("EEXIST: {:?}", String::from_utf8_lossy(name));
        }
        let name_str = std::str::from_utf8(name)
            .context("non-UTF-8 filename")?
            .to_string();
        let full_path = join_entry_path(&parent_path, &name_str);
        let ino = (st.nodes.len() as Inode) + 1;
        let now = unix_now();
        st.nodes.push(Node::File {
            source: FileSource::Dirty,
            size: 0,
            mtime_unix: now,
            path: full_path.clone(),
            mode: None,
        });
        st.dirty.insert(ino, Vec::new());
        st.children
            .entry(parent)
            .or_default()
            .push((name.to_vec(), ino, NodeKind::File));
        Ok(Attr {
            ino,
            size: 0,
            kind: NodeKind::File,
            mtime_unix: now,
            mode: None,
        })
    }

    /// Write `data` at `offset` to a file inode. Archive-backed bodies
    /// are promoted into the dirty journal on first write (copy-on-
    /// write). Returns the new file size.
    pub fn write(&self, ino: Inode, offset: u64, data: &[u8]) -> Result<u64> {
        self.require_writable()?;
        self.cow_to_dirty(ino)?;
        let mut st = self.state.write().unwrap();
        let body = st
            .dirty
            .get_mut(&ino)
            .ok_or_else(|| anyhow::anyhow!("dirty body missing for ino {}", ino))?;
        let end = offset as usize + data.len();
        if body.len() < end {
            body.resize(end, 0);
        }
        body[offset as usize..end].copy_from_slice(data);
        let size = body.len() as u64;
        // Update the node's recorded size + mtime.
        if let Some(Node::File { size: s, mtime_unix, .. }) = st.nodes.get_mut((ino - 1) as usize) {
            *s = size;
            *mtime_unix = unix_now();
        }
        Ok(size)
    }

    /// Resize a file to exactly `new_size`. Extends with zeros, or
    /// truncates. Archive-backed bodies are CoW'd first.
    pub fn truncate(&self, ino: Inode, new_size: u64) -> Result<()> {
        self.require_writable()?;
        self.cow_to_dirty(ino)?;
        let mut st = self.state.write().unwrap();
        let body = st
            .dirty
            .get_mut(&ino)
            .ok_or_else(|| anyhow::anyhow!("dirty body missing for ino {}", ino))?;
        body.resize(new_size as usize, 0);
        if let Some(Node::File { size, mtime_unix, .. }) = st.nodes.get_mut((ino - 1) as usize) {
            *size = new_size;
            *mtime_unix = unix_now();
        }
        Ok(())
    }

    /// Remove a file entry from `parent`. Archive-backed entries get
    /// added to the tombstone set; dirty-only entries just drop their
    /// body.
    pub fn unlink(&self, parent: Inode, name: &[u8]) -> Result<()> {
        self.require_writable()?;
        let mut st = self.state.write().unwrap();
        let siblings = st
            .children
            .get_mut(&parent)
            .ok_or_else(|| anyhow::anyhow!("no such parent ino {}", parent))?;
        let pos = siblings
            .iter()
            .position(|(n, _, kind)| n.as_slice() == name && *kind == NodeKind::File)
            .ok_or_else(|| anyhow::anyhow!("ENOENT or EISDIR for {:?}", String::from_utf8_lossy(name)))?;
        let (_, ino, _) = siblings.remove(pos);

        // Collect original path before mutating nodes.
        let (is_archive_backed, original_path) = match st.nodes.get((ino - 1) as usize) {
            Some(Node::File {
                source: FileSource::Archive(_),
                path,
                ..
            }) => (true, Some(path.clone())),
            _ => (false, None),
        };

        if is_archive_backed {
            if let Some(p) = original_path {
                st.tombstones.insert(p);
            }
        } else {
            st.dirty.remove(&ino);
        }
        // Invalidate the LRU for this inode.
        {
            let mut cache = self.cache.lock().unwrap();
            cache.invalidate(ino);
        }
        Ok(())
    }

    /// Create an empty directory under `parent`.
    pub fn mkdir(&self, parent: Inode, name: &[u8]) -> Result<Attr> {
        self.require_writable()?;
        let mut st = self.state.write().unwrap();
        let parent_path = match st.nodes.get((parent - 1) as usize) {
            Some(Node::Dir { path, .. }) => path.clone(),
            Some(_) => anyhow::bail!("parent ino {} is not a directory", parent),
            None => anyhow::bail!("no such parent inode {}", parent),
        };
        if let Some(siblings) = st.children.get(&parent) {
            if siblings.iter().any(|(n, _, _)| n.as_slice() == name) {
                anyhow::bail!("EEXIST: {:?}", String::from_utf8_lossy(name));
            }
        }
        let name_str = std::str::from_utf8(name).context("non-UTF-8 dirname")?.to_string();
        let full_path = join_entry_path(&parent_path, &name_str);
        let ino = (st.nodes.len() as Inode) + 1;
        let now = unix_now();
        st.nodes.push(Node::Dir {
            mtime_unix: now,
            path: full_path,
            mode: None,
        });
        st.children.insert(ino, Vec::new());
        st.children
            .entry(parent)
            .or_default()
            .push((name.to_vec(), ino, NodeKind::Dir));
        Ok(Attr {
            ino,
            size: 0,
            kind: NodeKind::Dir,
            mtime_unix: now,
            mode: None,
        })
    }

    /// Remove an empty directory from `parent`. Fails with `ENOTEMPTY`
    /// if the dir has any children.
    pub fn rmdir(&self, parent: Inode, name: &[u8]) -> Result<()> {
        self.require_writable()?;
        let mut st = self.state.write().unwrap();
        let siblings = st
            .children
            .get_mut(&parent)
            .ok_or_else(|| anyhow::anyhow!("no such parent inode {}", parent))?;
        let pos = siblings
            .iter()
            .position(|(n, _, kind)| n.as_slice() == name && *kind == NodeKind::Dir)
            .ok_or_else(|| anyhow::anyhow!("ENOENT or ENOTDIR for {:?}", String::from_utf8_lossy(name)))?;
        let (_, ino, _) = siblings[pos].clone();
        // Non-empty check.
        if st.children.get(&ino).map_or(false, |c| !c.is_empty()) {
            anyhow::bail!("ENOTEMPTY: {:?}", String::from_utf8_lossy(name));
        }
        st.children.get_mut(&parent).unwrap().remove(pos);
        st.children.remove(&ino);
        // We don't tombstone directories — ZIP directory markers are
        // optional; the rewritten archive will omit the dir if no
        // children live under it.
        Ok(())
    }

    /// Set the Unix mode bits for an inode. Persisted into the
    /// archive's `external_attr` field on commit (version_made_by
    /// already = 3 / Unix in the writer). Accepts the full permission
    /// word; type bits and anything outside 0o7777 are stripped.
    pub fn chmod(&self, ino: Inode, mode: u32) -> Result<()> {
        self.require_writable()?;
        let mut st = self.state.write().unwrap();
        let node = st
            .nodes
            .get_mut((ino - 1) as usize)
            .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
        node.set_mode_bits(mode & 0o7777);
        // Changing mode on an archive-backed file also forces a
        // rewrite of that entry — the mode lives in the CDR + LFH,
        // so we can't just byte-copy the original any more. CoW
        // promotes it.
        let is_archive_file = matches!(
            node,
            Node::File { source: FileSource::Archive(_), .. }
        );
        drop(st);
        if is_archive_file {
            self.cow_to_dirty(ino)?;
        }
        Ok(())
    }

    /// Set the last-modified timestamp for an inode. Persisted into
    /// the archive's DOS date/time fields on commit.
    pub fn set_mtime(&self, ino: Inode, mtime_unix: i64) -> Result<()> {
        self.require_writable()?;
        let mut st = self.state.write().unwrap();
        let node = st
            .nodes
            .get_mut((ino - 1) as usize)
            .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
        node.set_mtime(mtime_unix);
        // Same CoW story as chmod: the mtime lives in the LFH + CDR.
        let is_archive_file = matches!(
            node,
            Node::File { source: FileSource::Archive(_), .. }
        );
        drop(st);
        if is_archive_file {
            self.cow_to_dirty(ino)?;
        }
        Ok(())
    }

    /// Rename the child `src_name` of `src_parent` to `dst_name` under
    /// `dst_parent`. Fails with EEXIST if a sibling at the destination
    /// already exists. Files CoW through dirty on rename so the new
    /// path shows up on commit; directories traverse their subtree,
    /// CoWing every archive-backed descendant file.
    pub fn rename(
        &self,
        src_parent: Inode,
        src_name: &[u8],
        dst_parent: Inode,
        dst_name: &[u8],
    ) -> Result<()> {
        self.require_writable()?;

        // Find the child in src_parent.
        let (child_ino, child_kind) = {
            let st = self.state.read().unwrap();
            let siblings = st
                .children
                .get(&src_parent)
                .ok_or_else(|| anyhow::anyhow!("ENOENT: no src parent {}", src_parent))?;
            let (_, ino, kind) = siblings
                .iter()
                .find(|(n, _, _)| n.as_slice() == src_name)
                .ok_or_else(|| {
                    anyhow::anyhow!("ENOENT: {:?}", String::from_utf8_lossy(src_name))
                })?
                .clone();
            (ino, kind)
        };
        // Prevent moving root.
        if child_ino == ROOT_INO {
            anyhow::bail!("EINVAL: cannot rename root");
        }
        // Prevent moving a dir into its own descendant.
        if child_kind == NodeKind::Dir {
            self.ensure_not_ancestor(child_ino, dst_parent)?;
        }
        // Forbid overwriting existing dst for v1.
        {
            let st = self.state.read().unwrap();
            if let Some(siblings) = st.children.get(&dst_parent) {
                if siblings.iter().any(|(n, _, _)| n.as_slice() == dst_name) {
                    anyhow::bail!("EEXIST: {:?}", String::from_utf8_lossy(dst_name));
                }
            }
        }
        // Compute the new parent path.
        let dst_parent_path = {
            let st = self.state.read().unwrap();
            let p = st
                .nodes
                .get((dst_parent - 1) as usize)
                .ok_or_else(|| anyhow::anyhow!("ENOENT: no dst parent {}", dst_parent))?;
            match p {
                Node::Dir { path, .. } => path.clone(),
                Node::File { .. } => anyhow::bail!("ENOTDIR: dst parent {}", dst_parent),
            }
        };
        let new_name_str = std::str::from_utf8(dst_name)
            .context("non-UTF-8 rename target name")?
            .to_string();
        let new_path = join_entry_path(&dst_parent_path, &new_name_str);

        // Phase 1 — CoW any archive-backed file(s) that are affected.
        // For a file rename: just the file itself. For a dir rename:
        // every archive-backed file descendant.
        let to_cow: Vec<Inode> = match child_kind {
            NodeKind::File => {
                let st = self.state.read().unwrap();
                if let Some(Node::File { source: FileSource::Archive(_), .. }) =
                    st.nodes.get((child_ino - 1) as usize)
                {
                    vec![child_ino]
                } else {
                    vec![]
                }
            }
            NodeKind::Dir => self.collect_archive_backed_descendants(child_ino),
        };
        for ino in to_cow {
            self.cow_to_dirty(ino)?;
        }

        // Phase 2 — update children maps + node paths under the write lock.
        let mut st = self.state.write().unwrap();

        // Remove from src_parent's children.
        if let Some(siblings) = st.children.get_mut(&src_parent) {
            siblings.retain(|(n, _, _)| n.as_slice() != src_name);
        }
        // Add to dst_parent's children.
        st.children
            .entry(dst_parent)
            .or_default()
            .push((dst_name.to_vec(), child_ino, child_kind));

        // Update the renamed node's own path.
        let old_root_path = {
            let n = st.nodes.get((child_ino - 1) as usize).unwrap();
            n.path().to_string()
        };
        if let Some(n) = st.nodes.get_mut((child_ino - 1) as usize) {
            n.set_path(new_path.clone());
        }

        // For a dir: update every descendant's path by rewriting its
        // `old_root_path` prefix to `new_path`.
        if child_kind == NodeKind::Dir {
            let descendants = collect_descendant_inos_locked(&st, child_ino);
            for ino in descendants {
                let Some(node) = st.nodes.get_mut((ino - 1) as usize) else { continue };
                let old = node.path().to_string();
                if let Some(rest) = old.strip_prefix(&old_root_path) {
                    let new = format!("{}{}", new_path, rest);
                    node.set_path(new);
                }
            }
        }

        Ok(())
    }

    /// Bail if `ancestor_candidate` is inside the subtree rooted at
    /// `subtree_root`. Guards against `mv foo foo/bar`.
    fn ensure_not_ancestor(&self, subtree_root: Inode, candidate: Inode) -> Result<()> {
        if subtree_root == candidate {
            anyhow::bail!("EINVAL: cannot rename a directory into itself");
        }
        let st = self.state.read().unwrap();
        let descendants = collect_descendant_inos_locked(&st, subtree_root);
        if descendants.contains(&candidate) {
            anyhow::bail!("EINVAL: rename target is inside the source directory");
        }
        Ok(())
    }

    /// Walk `root`'s subtree and return inodes of every archive-backed
    /// File descendant. Taken outside a lock so each one can CoW
    /// without holding the write lock.
    fn collect_archive_backed_descendants(&self, root: Inode) -> Vec<Inode> {
        let st = self.state.read().unwrap();
        let inos = collect_descendant_inos_locked(&st, root);
        inos.into_iter()
            .filter(|ino| {
                matches!(
                    st.nodes.get((*ino - 1) as usize),
                    Some(Node::File { source: FileSource::Archive(_), .. })
                )
            })
            .collect()
    }

    fn require_writable(&self) -> Result<()> {
        if !self.writable {
            anyhow::bail!("filesystem is read-only (mount with --write to enable)");
        }
        Ok(())
    }

    /// Promote an archive-backed file to a dirty entry so subsequent
    /// writes have somewhere to land. The original archive entry is
    /// tombstoned so commit doesn't double-emit it.
    fn cow_to_dirty(&self, ino: Inode) -> Result<()> {
        // Fast exit if already dirty.
        {
            let st = self.state.read().unwrap();
            let node = st
                .nodes
                .get((ino - 1) as usize)
                .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
            if let Node::File { source: FileSource::Dirty, .. } = node {
                return Ok(());
            }
        }

        // Read the current body under the archive-backed path.
        let (entry_idx, original_path) = {
            let st = self.state.read().unwrap();
            let node = st
                .nodes
                .get((ino - 1) as usize)
                .ok_or_else(|| anyhow::anyhow!("no such inode {}", ino))?;
            match node {
                Node::File {
                    source: FileSource::Archive(idx),
                    path,
                    ..
                } => (*idx, path.clone()),
                Node::File { source: FileSource::Dirty, .. } => return Ok(()),
                Node::Dir { .. } => anyhow::bail!("cow_to_dirty called on directory ino {}", ino),
            }
        };
        let body = self.decompress_entry(entry_idx)?;

        // Promote.
        let mut st = self.state.write().unwrap();
        st.dirty.insert(ino, body);
        st.tombstones.insert(original_path);
        if let Some(Node::File { source, .. }) = st.nodes.get_mut((ino - 1) as usize) {
            *source = FileSource::Dirty;
        }
        {
            let mut cache = self.cache.lock().unwrap();
            cache.invalidate(ino);
        }
        Ok(())
    }

    // ── Commit ────────────────────────────────────────────────────────────

    /// Flush all pending changes to the on-disk archive via `pipeline::run`
    /// with `update + atomic` semantics. Returns a summary. Safe to call
    /// on a read-only mount (no-op) or a writable mount with no changes
    /// (no-op).
    pub fn commit(&self) -> Result<CommitSummary> {
        if !self.writable {
            return Ok(CommitSummary::default());
        }
        let (dirty_specs, tombstones, archive_path) = {
            let st = self.state.read().unwrap();
            let dirty: Vec<DirtyCommitSpec> = st
                .dirty
                .iter()
                .filter_map(|(ino, body)| {
                    let node = st.nodes.get((*ino - 1) as usize)?;
                    if let Node::File {
                        path,
                        mtime_unix,
                        mode,
                        ..
                    } = node
                    {
                        Some(DirtyCommitSpec {
                            path: path.clone(),
                            body: body.clone(),
                            mtime_unix: *mtime_unix,
                            mode: *mode,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            let tombstones: Vec<String> = st.tombstones.iter().cloned().collect();
            (dirty, tombstones, self.archive_path.clone())
        };
        if dirty_specs.is_empty() && tombstones.is_empty() {
            return Ok(CommitSummary::default());
        }

        // Materialise dirty bodies to a staging tempdir so the update
        // engine can consume them via its normal filesystem path. Set
        // each tempfile's mode + mtime so the walker picks them up
        // and the writer emits them as `external_attr` + DOS time.
        let staging = tempfile::tempdir().context("create commit staging tempdir")?;
        let n_written = dirty_specs.len();
        for spec in dirty_specs {
            let dest = staging.path().join(&spec.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create staging parent {}", parent.display())
                })?;
            }
            std::fs::write(&dest, &spec.body)
                .with_context(|| format!("write staging body for {}", spec.path))?;

            // Apply user-set permission bits (chmod from mount) if any.
            // Masked to 0o7777 so we only touch the permission portion.
            #[cfg(unix)]
            if let Some(m) = spec.mode {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &dest,
                    std::fs::Permissions::from_mode(m & 0o7777),
                );
            }
            // Apply the entry's mtime so the walker's DOS mtime computation
            // picks it up. We don't bother with atime.
            if spec.mtime_unix > 0 {
                let _ = set_mtime_seconds(&dest, spec.mtime_unix);
            }
            // Keep the compiler quiet about `spec.mode` on non-Unix.
            let _ = spec.mode;
        }

        let n_tombstones = tombstones.len();

        // Trick to get the walker's archive-entry naming right: cd into
        // staging and pass `.` as the input path with basename_only=true.
        // `archive_prefix_for_root(Path::new("."), true)` returns ""
        // (file_name of "." is None), and `archive_name` then yields the
        // relative path verbatim — which is exactly our archive entry
        // name. Without this, walker prepends the staging dir's
        // absolute path (sans leading /), so e.g. "corpus/new_file.txt"
        // would land as "tmp/.tmpXXX/corpus/new_file.txt" in the archive.
        let original_cwd =
            std::env::current_dir().context("record cwd for commit chdir")?;
        std::env::set_current_dir(staging.path()).with_context(|| {
            format!("chdir into commit staging {}", staging.path().display())
        })?;
        let opts = self.commit_options(&archive_path, std::path::Path::new("."), tombstones)?;
        // `tzip u` dispatch at this layer: `opts.update == true` and
        // the archive exists (otherwise we'd be creating from a FUSE
        // mount, which is nonsensical). Call `update::run_update`
        // directly rather than going through `pipeline::run` so the
        // create-mode pipeline doesn't have to know about the update
        // path (previously was `pipeline → update`, which fed the
        // module cycle the deslop-please lint fixed).
        let run_result = if opts.update && opts.archive.exists() {
            crate::update::run_update(opts)
        } else {
            crate::pipeline::run(opts)
        };
        // Always restore cwd, even on error.
        let _ = std::env::set_current_dir(&original_cwd);
        run_result.context("commit writeback")?;

        if self.verbose {
            eprintln!(
                "tzip mount: committed {} dirty entries, {} deletions to {}",
                n_written,
                n_tombstones,
                archive_path.display()
            );
        }
        Ok(CommitSummary {
            dirty: n_written,
            tombstoned: n_tombstones,
        })
    }

    fn commit_options(
        &self,
        archive: &std::path::Path,
        staging: &std::path::Path,
        delete: Vec<String>,
    ) -> Result<Options> {
        let cpu_jobs = num_cpus::get_physical().max(1);
        Ok(Options {
            archive: archive.to_path_buf(),
            paths: vec![staging.to_path_buf()],
            method: Method::Deflate,
            level: 6,
            cpu_jobs,
            read_jobs: 2,
            walk_jobs: cpu_jobs.min(4).max(1),
            password: self.password.clone(),
            sort: false,
            exclude: vec![],
            quiet: !self.verbose,
            verbose: self.verbose,
            keep_cache: false,
            streaming: false,
            solid: false,
            tui: false,
            classic_walk: false,
            dispatch_io: false,
            usb_info: false,
            warn_contention: false,
            #[cfg(any(
                all(feature = "raw-apfs", target_os = "macos"),
                target_os = "linux"
            ))]
            raw_block: false,
            #[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
            apfs_device: None,
            #[cfg(all(feature = "io-uring", target_os = "linux"))]
            io_uring: false,
            // `true` so `archive_prefix_for_root(".", true)` returns "".
            // See the chdir comment above for the full story.
            basename_only: true,
            user_flags: UserSetFlags::default(),
            update: true,
            delete,
            atomic: true,
            write_md5: false,
            write_sha256: false,
            index_names: false,
            index_terms: Vec::new(),
            facet: Vec::new(),
            write_columnar: None,
        })
    }
}

/// Summary of what `commit()` wrote through to the archive.
#[derive(Default, Debug, Clone, Copy)]
pub struct CommitSummary {
    pub dirty: usize,
    pub tombstoned: usize,
}

/// BFS the children map, collecting every descendant inode of `root`
/// (not including `root` itself). Caller must already hold the read
/// lock on MutableState.
fn collect_descendant_inos_locked(st: &MutableState, root: Inode) -> Vec<Inode> {
    let mut out: Vec<Inode> = Vec::new();
    let mut stack: Vec<Inode> = vec![root];
    while let Some(current) = stack.pop() {
        let Some(siblings) = st.children.get(&current) else { continue };
        for (_, ino, _) in siblings {
            out.push(*ino);
            stack.push(*ino);
        }
    }
    out
}

fn node_attr(ino: Inode, n: &Node) -> Attr {
    match n {
        Node::Dir { mtime_unix, mode, .. } => Attr {
            ino,
            size: 0,
            kind: NodeKind::Dir,
            mtime_unix: *mtime_unix,
            mode: *mode,
        },
        Node::File { size, mtime_unix, mode, .. } => Attr {
            ino,
            size: *size,
            kind: NodeKind::File,
            mtime_unix: *mtime_unix,
            mode: *mode,
        },
    }
}

/// Join a parent directory's path with a new child name, producing a
/// well-formed archive-relative path. Root → just the name.
fn join_entry_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent_path, name)
    }
}

/// Builds the inode + children maps from the archive's CDR.
/// Synthetic directories are created for every intermediate path
/// component. Entries whose name ends in `/` are treated as explicit
/// directory markers.
fn build_tree(archive: &Archive) -> (Vec<Node>, HashMap<Inode, Vec<(Vec<u8>, Inode, NodeKind)>>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut children: HashMap<Inode, Vec<(Vec<u8>, Inode, NodeKind)>> = HashMap::new();
    nodes.push(Node::Dir {
        mtime_unix: 0,
        path: String::new(),
        mode: None,
    });
    children.insert(ROOT_INO, Vec::new());

    let mut dir_by_path: HashMap<Vec<u8>, Inode> = HashMap::new();

    for (idx, entry) in archive.iter_entries().enumerate() {
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
                    if dir_by_path.get(&dir_key).is_none() {
                        let ino = nodes.len() as Inode + 1;
                        nodes.push(Node::Dir {
                            mtime_unix: dos_to_unix(entry.mtime),
                            path: running.clone(),
                            mode: None,
                        });
                        children.insert(ino, Vec::new());
                        children
                            .get_mut(&parent_ino)
                            .unwrap()
                            .push((part.as_bytes().to_vec(), ino, NodeKind::Dir));
                        dir_by_path.insert(dir_key, ino);
                    }
                } else {
                    let ino = nodes.len() as Inode + 1;
                    nodes.push(Node::File {
                        source: FileSource::Archive(idx),
                        size: entry.uncomp_size,
                        mtime_unix: dos_to_unix(entry.mtime),
                        path: running.clone(),
                        mode: None,
                    });
                    children
                        .get_mut(&parent_ino)
                        .unwrap()
                        .push((part.as_bytes().to_vec(), ino, NodeKind::File));
                }
            } else {
                let ino = if let Some(&existing) = dir_by_path.get(&dir_key) {
                    existing
                } else {
                    let ino = nodes.len() as Inode + 1;
                    nodes.push(Node::Dir {
                        mtime_unix: 0,
                        path: running.clone(),
                        mode: None,
                    });
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

use crate::dos_time::dos_to_unix;

/// Local record used by `commit()` — materialise a dirty entry's path,
/// body, and VFS-side metadata (mtime + optional chmod) in a single
/// pass under the state lock, then process outside.
struct DirtyCommitSpec {
    path: String,
    body: Vec<u8>,
    mtime_unix: i64,
    mode: Option<u32>,
}

/// Set a file's mtime to `ts` Unix seconds via `File::set_modified`.
/// Portable across Unix + Windows; opens the file read-write to get
/// a handle without truncating.
fn set_mtime_seconds(path: &std::path::Path, ts: i64) -> std::io::Result<()> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    if ts <= 0 {
        return Ok(());
    }
    let when: SystemTime = UNIX_EPOCH + Duration::from_secs(ts as u64);
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_modified(when)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
