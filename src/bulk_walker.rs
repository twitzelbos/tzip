//! macOS `getattrlistbulk` directory walker.
//!
//! Motivation: on APFS a `readdir + stat` pair does N + N metadata reads per
//! directory. `getattrlistbulk` returns dozens of entries with their
//! attributes already filled in — one syscall per batch instead of one per
//! entry. On tools that measure it (`ls`, `find`, Finder), it's 3-10× faster
//! for cold-cache directory enumeration.
//!
//! Layout of the returned buffer per Apple's `<sys/attr.h>` docs and
//! [the getattrlist(2) manpage](x-man-page://2/getattrlist):
//!
//!     entry := u32 length | requested attributes in canonical bit order
//!
//! For each requested `commonattr` / `fileattr` bit that's set, an
//! attribute-typed value appears at its natural alignment within the entry.
//! Variable-length values (strings) are indirected via `attrreference_t`,
//! which stores an offset (relative to the reference's own address) and a
//! length; the string data lives later in the same entry.
//!
//! We request a fixed, minimal set of attributes and parse them in the exact
//! order Apple guarantees:
//!
//!   ATTR_CMN_RETURNED_ATTRS  → attribute_set_t   (5×u32, 20 bytes) — always first
//!   ATTR_CMN_NAME            → attrreference_t   (8 bytes) + name data (later)
//!   ATTR_CMN_OBJTYPE         → fsobj_type_t      (u32, 4 bytes)
//!   ATTR_CMN_MODTIME         → timespec          (16 bytes on 64-bit macOS)
//!   ATTR_FILE_TOTALSIZE      → off_t             (u64, 8 bytes) — only present for files
//!
//! Directories don't get a TOTALSIZE (fileattr) — we detect via OBJTYPE.

#![cfg(target_os = "macos")]

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use std::ffi::{CString, OsStr, OsString};
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::platform::OwnedDirFd;
use crate::walker::WorkItem;

// fsobj_type_t values (from <sys/vnode.h>).
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// Streaming bulk walk. Sends every discovered regular file into `tx` as a
/// `WorkItem`, with an `Arc<OwnedDirFd>` pointing at the file's parent
/// directory so downstream readers can `openat` it.
pub fn walk_stream_bulk(
    roots: &[PathBuf],
    exclude: &[String],
    tx: Sender<WorkItem>,
) -> Result<()> {
    for root in roots {
        let root_canon = root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", root.display()))?;

        if root_canon.is_file() {
            // Fall back to a plain stat for single-file roots — bulk walk is
            // for directories.
            let meta = std::fs::metadata(&root_canon)?;
            let name = root_canon
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_excluded(&name, exclude) {
                continue;
            }
            let mtime = dos_time_from_meta(&meta);
            let item = WorkItem {
                path: root_canon.clone(),
                name_in_archive: name,
                size: meta.len(),
                mtime,
                dirfd: None,
                basename: None,
            };
            if tx.send(item).is_err() {
                return Ok(());
            }
            continue;
        }

        let base_name = root_canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        walk_recursive(&root_canon, &base_name, exclude, &tx)?;
    }
    Ok(())
}

fn walk_recursive(
    dir: &Path,
    archive_prefix: &str,
    exclude: &[String],
    tx: &Sender<WorkItem>,
) -> Result<()> {
    let dirfd = Arc::new(OwnedDirFd::open(dir).with_context(|| format!("open {}", dir.display()))?);

    // 64 KiB fits several hundred entries with our attribute layout.
    let mut buf = vec![0u8; 64 * 1024];

    let mut attrs: libc::attrlist = unsafe { mem::zeroed() };
    attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    // NB: MODTIME is deliberately omitted — variable-width attribute alignment
    // in the getattrlistbulk output is fiddly and a partial parse was producing
    // garbage DOS dates in a first pass. Archives from the bulk walker set
    // mtime to the DOS epoch (1980-01-01). Users who need real per-file mtimes
    // can pass `--classic-walk`. TODO: parse MODTIME with proper alignment.
    attrs.commonattr =
        libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE;
    attrs.fileattr = libc::ATTR_FILE_TOTALSIZE;

    // Sub-directories collected here so we can descend after finishing the
    // current directory (avoids holding many dirfds open simultaneously in
    // deep trees).
    let mut subdirs: Vec<PathBuf> = Vec::new();

    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                dirfd.as_raw(),
                &mut attrs as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("getattrlistbulk {}", dir.display()));
        }
        if n == 0 {
            break;
        }

        let mut offset: usize = 0;
        for _ in 0..n {
            let entry_start = offset;
            let entry_len =
                u32::from_ne_bytes(buf[entry_start..entry_start + 4].try_into().unwrap()) as usize;
            // Guard: never read past this record.
            let entry_end = entry_start + entry_len;
            let mut p = entry_start + 4;

            // Field 1: ATTR_CMN_RETURNED_ATTRS (attribute_set_t = 5 × u32 = 20 bytes)
            let returned_common = u32::from_ne_bytes(buf[p..p + 4].try_into().unwrap());
            let returned_file =
                u32::from_ne_bytes(buf[p + 12..p + 16].try_into().unwrap());
            p += 20;

            // Field 2: ATTR_CMN_NAME → attrreference_t (i32 offset + u32 length)
            let name = if returned_common & libc::ATTR_CMN_NAME != 0 {
                let name_ref_off = p;
                let name_offset_from_ref =
                    i32::from_ne_bytes(buf[p..p + 4].try_into().unwrap());
                let name_length =
                    u32::from_ne_bytes(buf[p + 4..p + 8].try_into().unwrap()) as usize;
                p += 8;
                let name_start = (name_ref_off as isize + name_offset_from_ref as isize) as usize;
                // name_length includes the trailing NUL.
                let name_bytes = &buf[name_start..name_start + name_length.saturating_sub(1)];
                OsStr::from_bytes(name_bytes).to_os_string()
            } else {
                offset = entry_end;
                continue;
            };

            // Field 3: ATTR_CMN_OBJTYPE (fsobj_type_t = u32)
            let objtype = if returned_common & libc::ATTR_CMN_OBJTYPE != 0 {
                let v = u32::from_ne_bytes(buf[p..p + 4].try_into().unwrap());
                p += 4;
                v
            } else {
                // Unknown type — skip.
                offset = entry_end;
                continue;
            };

            // Field 4: ATTR_FILE_TOTALSIZE (u64) — only present when the
            // entry is a regular file. Aligned to 8 within the buffer.
            let size = if objtype == VREG && (returned_file & libc::ATTR_FILE_TOTALSIZE) != 0 {
                if p % 8 != 0 {
                    p += 8 - (p % 8);
                }
                u64::from_ne_bytes(buf[p..p + 8].try_into().unwrap())
            } else {
                0
            };
            let mtime_sec: i64 = 0; // see comment on attrs.commonattr above

            // Advance to next record regardless of what we do with this one.
            offset = entry_end;

            // Skip "." / ".."
            if name.as_bytes() == b"." || name.as_bytes() == b".." {
                continue;
            }

            match objtype {
                VREG => {
                    let name_string = name.to_string_lossy().into_owned();
                    let archive_name = if archive_prefix.is_empty() {
                        name_string.clone()
                    } else {
                        format!("{}/{}", archive_prefix, name_string)
                    };
                    if is_excluded(&archive_name, exclude) || is_excluded(&name_string, exclude) {
                        continue;
                    }
                    let full_path = dir.join(&name);
                    let mtime = dos_time_from_unix(mtime_sec);
                    let item = WorkItem {
                        path: full_path,
                        name_in_archive: archive_name,
                        size,
                        mtime,
                        dirfd: Some(Arc::clone(&dirfd)),
                        basename: Some(name),
                    };
                    if tx.send(item).is_err() {
                        return Ok(());
                    }
                }
                VDIR => {
                    subdirs.push(dir.join(&name));
                }
                VLNK => {
                    // symlinks: skip (matches jwalk's `follow_links(false)`)
                }
                _ => {
                    // sockets, devices, etc — skip
                }
            }
        }
    }

    drop(buf);
    // dirfd stays alive while any WorkItem we sent still references it,
    // which is exactly what we want for `openat` downstream.

    // Recurse into subdirs after finishing this directory's stream so we
    // don't stack too many open dirfds.
    for sub in subdirs {
        let sub_name = sub.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let child_prefix = if archive_prefix.is_empty() {
            sub_name
        } else if sub_name.is_empty() {
            archive_prefix.to_string()
        } else {
            format!("{}/{}", archive_prefix, sub_name)
        };
        walk_recursive(&sub, &child_prefix, exclude, tx)?;
    }
    Ok(())
}

pub(crate) fn is_excluded(name: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| glob_match(g, name))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn recur(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if recur(&p[1..], t) {
                    return true;
                }
                if let Some((_, rest)) = t.split_first() {
                    return recur(p, rest);
                }
                false
            }
            (Some(b'?'), Some(_)) => recur(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => recur(&p[1..], &t[1..]),
            _ => false,
        }
    }
    recur(pattern.as_bytes(), text.as_bytes())
}

fn dos_time_from_meta(meta: &std::fs::Metadata) -> (u16, u16) {
    use std::time::UNIX_EPOCH;
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    dos_time_from_unix(secs)
}

pub(crate) fn dos_time_from_unix(unix: i64) -> (u16, u16) {
    let secs = if unix < 0 { 0 } else { unix };
    let days = secs / 86_400;
    let sec_of_day = (secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let year = y as i32;
    if year < 1980 {
        return (((1 << 9) | (1 << 5)) as u16, 0);
    }
    let dos_date = (((year - 1980) as u16) << 9) | ((m as u16) << 5) | (d as u16);
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day / 60) % 60;
    let sec2 = (sec_of_day % 60) / 2;
    let dos_time = ((hour as u16) << 11) | ((minute as u16) << 5) | (sec2 as u16);
    (dos_date, dos_time)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// Unused but kept for potential future openat-relative walks.
#[allow(dead_code)]
fn cstr(s: &OsStr) -> Result<CString> {
    CString::new(s.as_bytes()).map_err(|_| anyhow!("path contains NUL byte"))
}

#[allow(dead_code)]
fn as_string(name: &OsString) -> String {
    name.to_string_lossy().into_owned()
}
