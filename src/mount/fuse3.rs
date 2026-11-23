//! Linux FUSE3 adapter — thin shim between `fuser::Filesystem` and the
//! platform-agnostic `VirtualFs`.

use anyhow::{Context, Result};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, Request,
};
use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
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

    let vfs = Arc::new(VirtualFs::new(
        archive,
        opts.password.clone(),
        opts.cache_mb,
        opts.verbose,
    )?);
    let fs = TzipFs { vfs };

    let mut mount_opts = vec![
        MountOption::RO,
        MountOption::FSName("tzip".into()),
        MountOption::Subtype("tzip".into()),
    ];
    if opts.allow_other {
        mount_opts.push(MountOption::AllowOther);
    }

    if !opts.quiet {
        eprintln!(
            "tzip mount: {} → {}",
            opts.archive_spec,
            opts.mountpoint.display()
        );
        eprintln!(
            "tzip mount: Ctrl-C or `fusermount3 -u {}` to unmount",
            opts.mountpoint.display()
        );
    }

    let mut session = fuser::Session::new(fs, &opts.mountpoint, &mount_opts)
        .context("fuser Session::new (is FUSE3 installed?)")?;
    let mut unmounter = session.unmount_callable();

    // Signal handler: SIGINT / SIGTERM → trigger clean unmount so
    // session.run() returns.
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

    // External umount paths (fusermount3 -u) would make run() return on
    // their own — close the signal handle so the watcher thread exits
    // cleanly whether we got here via signal or external unmount.
    signals_handle.close();
    let _ = watcher.join();

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
            Some(attr) => reply.entry(&TTL, &to_file_attr(attr), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match self.vfs.getattr(ino) {
            Some(attr) => reply.attr(&TTL, &to_file_attr(attr)),
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
        // FUSE's readdir needs "." and ".." as the first two synthetic
        // entries. offset is 1-indexed; we emit entry i at offset i+1.
        let synthetic = [
            (ino, FileType::Directory, b".".to_vec()),
            // ".." normally points at the parent. We don't track parent
            // inodes; for the root, it's itself, which is what FUSE
            // typically expects.
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

fn to_file_attr(a: Attr) -> FileAttr {
    let time = unix_to_system(a.mtime_unix);
    let (kind, perm, nlink) = match a.kind {
        NodeKind::Dir => (FileType::Directory, 0o555, 2),
        NodeKind::File => (FileType::RegularFile, 0o444, 1),
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

#[allow(dead_code)]
fn _root_ino_is_one() -> u64 {
    ROOT_INO
}
