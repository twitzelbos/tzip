//! Linux FUSE3 adapter — thin shim between `fuser::Filesystem` and the
//! platform-agnostic `VirtualFs`.
//!
//! Two operating modes:
//!   - **Read-only (default)**: serves getattr/lookup/readdir/read.
//!   - **Writable (`--write`)**: additionally serves create/write/
//!     unlink/mkdir/rmdir/setattr(size). All edits are journaled in
//!     memory inside `VirtualFs`; the backing archive is untouched
//!     until this adapter calls `vfs.commit()` on unmount. Commit
//!     delegates to `pipeline::run` with `update + atomic` semantics,
//!     so a crash before unmount loses in-memory changes but never
//!     corrupts the archive.

use anyhow::{bail, Context, Result};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyWrite, Request,
};
use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::vfs::{Attr, NodeKind, VirtualFs, ROOT_INO};
use super::MountOptions;
use crate::reader;

const TTL: Duration = Duration::from_secs(1);

pub fn mount(opts: MountOptions) -> Result<()> {
    // Open the archive once using the usual local/URL/S3 dispatch.
    let archive = reader::open_archive(&opts.archive_spec)
        .with_context(|| format!("open archive {}", opts.archive_spec))?;

    // Writable is a local-only capability: we need to be able to call
    // `pipeline::run` with atomic rename against a real filesystem
    // path. HTTP / S3 archives stay read-only even if --write is set.
    let is_remote = opts.archive_spec.starts_with("http://")
        || opts.archive_spec.starts_with("https://")
        || opts.archive_spec.starts_with("s3://");
    let writable = opts.write && !is_remote;
    if opts.write && is_remote && !opts.quiet {
        eprintln!(
            "tzip mount: --write ignored for remote archive; mount is read-only"
        );
    }

    let archive_path = if writable {
        PathBuf::from(&opts.archive_spec)
    } else {
        PathBuf::new()
    };

    let vfs = Arc::new(VirtualFs::new_full(
        archive,
        opts.password.clone(),
        opts.cache_mb,
        opts.verbose,
        writable,
        archive_path,
    )?);
    let fs = TzipFs { vfs: Arc::clone(&vfs) };

    let mut mount_opts = vec![
        if writable { MountOption::RW } else { MountOption::RO },
        MountOption::FSName("tzip".into()),
        MountOption::Subtype("tzip".into()),
    ];
    if opts.allow_other {
        mount_opts.push(MountOption::AllowOther);
    }

    if !opts.quiet {
        eprintln!(
            "tzip mount: {} \u{2192} {} ({})",
            opts.archive_spec,
            opts.mountpoint.display(),
            if writable { "rw" } else { "ro" },
        );
        eprintln!(
            "tzip mount: Ctrl-C or `fusermount3 -u {}` to unmount",
            opts.mountpoint.display()
        );
        if writable {
            eprintln!(
                "tzip mount: --write mode — pending changes commit on unmount"
            );
        }
    }

    let mut session = fuser::Session::new(fs, &opts.mountpoint, &mount_opts)
        .context("fuser Session::new (is FUSE3 installed?)")?;
    let mut unmounter = session.unmount_callable();

    let signals = Signals::new([SIGINT, SIGTERM])
        .context("register signal handler")?;
    let signals_handle = signals.handle();
    let watcher = thread::spawn(move || {
        let mut signals = signals;
        for sig in signals.forever() {
            eprintln!("tzip mount: received signal {}, unmounting", sig);
            let _ = unmounter.unmount();
            break;
        }
    });

    let run_result = session.run().context("fuser session.run()");

    signals_handle.close();
    let _ = watcher.join();

    // Commit any pending writes. On commit failure we don't delete the
    // original archive — `pipeline::run` in atomic mode rebuilds to a
    // temp file and renames into place only on success, so the worst
    // case is "nothing changed on disk and we yelled about it".
    if writable {
        match vfs.commit() {
            Ok(summary) => {
                if !opts.quiet && (summary.dirty > 0 || summary.tombstoned > 0) {
                    eprintln!(
                        "tzip mount: committed {} new/changed entries, {} deletions",
                        summary.dirty, summary.tombstoned
                    );
                }
            }
            Err(e) => {
                eprintln!("tzip mount: commit FAILED — pending changes not written: {:#}", e);
                return Err(e);
            }
        }
    }

    if !opts.quiet {
        eprintln!("tzip mount: unmounted");
    }
    run_result
}

struct TzipFs {
    vfs: Arc<VirtualFs>,
}

impl Filesystem for TzipFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.vfs.lookup(parent, name.as_bytes()) {
            Some(attr) => reply.entry(&TTL, &to_file_attr(attr, self.vfs.is_writable()), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match self.vfs.getattr(ino) {
            Some(attr) => reply.attr(&TTL, &to_file_attr(attr, self.vfs.is_writable())),
            None => reply.error(libc::ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // We only act on the size field (truncate). Chmod / chown /
        // timestamp changes are accepted as no-ops so tools like
        // `install -m`, `touch`, and `cp -p` don't error — but we
        // don't actually persist them (tzip's writer emits a fixed
        // mode and uses now() for mtime).
        if let Some(new_size) = size {
            if let Err(e) = self.vfs.truncate(ino, new_size) {
                eprintln!("tzip mount: truncate ino={} err: {:#}", ino, e);
                reply.error(libc::EIO);
                return;
            }
        }
        match self.vfs.getattr(ino) {
            Some(attr) => reply.attr(&TTL, &to_file_attr(attr, self.vfs.is_writable())),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        match self.vfs.getattr(ino) {
            Some(a) if a.kind == NodeKind::File => reply.opened(0, 0),
            Some(_) => reply.error(libc::EISDIR),
            None => reply.error(libc::ENOENT),
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.vfs.create(parent, name.as_bytes()) {
            Ok(attr) => reply.created(
                &TTL,
                &to_file_attr(attr, self.vfs.is_writable()),
                0,
                0,
                0,
            ),
            Err(e) => {
                let msg = format!("{}", e);
                let errno = if msg.contains("EEXIST") {
                    libc::EEXIST
                } else if msg.contains("read-only") {
                    libc::EROFS
                } else {
                    eprintln!("tzip mount: create {:?} err: {:#}", name, e);
                    libc::EIO
                };
                reply.error(errno);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.vfs.write(ino, offset as u64, data) {
            Ok(_new_size) => reply.written(data.len() as u32),
            Err(e) => {
                let msg = format!("{}", e);
                let errno = if msg.contains("read-only") {
                    libc::EROFS
                } else {
                    eprintln!("tzip mount: write ino={} err: {:#}", ino, e);
                    libc::EIO
                };
                reply.error(errno);
            }
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.vfs.unlink(parent, name.as_bytes()) {
            Ok(()) => reply.ok(),
            Err(e) => {
                let msg = format!("{}", e);
                let errno = if msg.contains("ENOENT") || msg.contains("EISDIR") {
                    libc::ENOENT
                } else if msg.contains("read-only") {
                    libc::EROFS
                } else {
                    eprintln!("tzip mount: unlink {:?} err: {:#}", name, e);
                    libc::EIO
                };
                reply.error(errno);
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.vfs.mkdir(parent, name.as_bytes()) {
            Ok(attr) => reply.entry(&TTL, &to_file_attr(attr, self.vfs.is_writable()), 0),
            Err(e) => {
                let msg = format!("{}", e);
                let errno = if msg.contains("EEXIST") {
                    libc::EEXIST
                } else if msg.contains("read-only") {
                    libc::EROFS
                } else {
                    eprintln!("tzip mount: mkdir {:?} err: {:#}", name, e);
                    libc::EIO
                };
                reply.error(errno);
            }
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.vfs.rmdir(parent, name.as_bytes()) {
            Ok(()) => reply.ok(),
            Err(e) => {
                let msg = format!("{}", e);
                let errno = if msg.contains("ENOTEMPTY") {
                    libc::ENOTEMPTY
                } else if msg.contains("ENOENT") || msg.contains("ENOTDIR") {
                    libc::ENOENT
                } else if msg.contains("read-only") {
                    libc::EROFS
                } else {
                    eprintln!("tzip mount: rmdir {:?} err: {:#}", name, e);
                    libc::EIO
                };
                reply.error(errno);
            }
        }
    }

    fn fsync(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Per-file fsync is a no-op — commit happens at unmount. This
        // avoids partial-commit states on a running mount at the cost
        // of making `sync(1)` not actually persist mid-mount.
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.vfs.read(ino, offset as u64, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => {
                eprintln!("tzip mount: read ino={} err: {:#}", ino, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        match self.vfs.getattr(ino) {
            Some(a) if a.kind == NodeKind::Dir => reply.opened(0, 0),
            Some(_) => reply.error(libc::ENOTDIR),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.vfs.readdir(ino) {
            Some(e) => e,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let synthetic = [
            (ino, FileType::Directory, b".".to_vec()),
            (ino, FileType::Directory, b"..".to_vec()),
        ];
        let mut cur_offset = 1i64;
        for (child_ino, _kind, name) in synthetic {
            if offset < cur_offset {
                if reply.add(child_ino, cur_offset, FileType::Directory, OsStr::from_bytes(&name)) {
                    reply.ok();
                    return;
                }
            }
            cur_offset += 1;
        }
        for (child_ino, kind, name) in entries {
            if offset < cur_offset {
                let ft = match kind {
                    NodeKind::Dir => FileType::Directory,
                    NodeKind::File => FileType::RegularFile,
                };
                if reply.add(child_ino, cur_offset, ft, OsStr::from_bytes(&name)) {
                    reply.ok();
                    return;
                }
            }
            cur_offset += 1;
        }
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

fn to_file_attr(a: Attr, writable: bool) -> FileAttr {
    let time = unix_to_system(a.mtime_unix);
    let (kind, perm, nlink) = match a.kind {
        NodeKind::Dir => {
            let p = if writable { 0o755 } else { 0o555 };
            (FileType::Directory, p, 2)
        }
        NodeKind::File => {
            let p = if writable { 0o644 } else { 0o444 };
            (FileType::RegularFile, p, 1)
        }
    };
    FileAttr {
        ino: a.ino,
        size: a.size,
        blocks: (a.size + 511) / 512,
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind,
        perm,
        nlink,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn unix_to_system(ts: i64) -> SystemTime {
    if ts <= 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_secs(ts as u64)
}

// Silence an otherwise-unused helper that the rest of the module no
// longer references.
#[allow(dead_code)]
fn _root_ino_is_one() -> u64 {
    ROOT_INO
}

// Silence an otherwise-unused import (anyhow::bail) that we may or
// may not touch depending on which error branches are compiled.
#[allow(dead_code)]
fn _touch_bail() -> anyhow::Result<()> {
    bail!("placeholder");
}
