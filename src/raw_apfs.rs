//! Raw APFS block reader — bypasses the VFS + any on-access AV.
//!
//! Enabled with the Cargo feature `raw-apfs`. Uses the vendored `apfs`
//! parser (see `vendor/dpp/apfs`, MIT) to walk an APFS volume by reading
//! `/dev/rdiskN` directly. This avoids the per-file `open()` syscall
//! that Sophos/CrowdStrike/etc. hook via Endpoint Security.
//!
//! See [`docs/RAW_BLOCK.md`](../../docs/RAW_BLOCK.md) for the full
//! design + testing notes.
//!
//! **Access requirements** (`/dev/rdisk*` is root:operator 0640):
//! - `sudo tzip …` — per-invocation elevation, or
//! - `sudo dseditgroup -o edit -a $USER -t user operator` + reboot — persistent
//!
//! **Supported volumes:**
//! - Unencrypted external APFS drives ✓  (tzip's actual target)
//! - Unencrypted internal APFS volumes ✓
//! - Sealed system volumes ✓ (they're read-only and structurally stable)
//!
//! **NOT supported — FileVault-encrypted volumes:**
//! On FileVault-enabled volumes, the container/volume superblocks are
//! stored unencrypted (they must be, to bootstrap decryption) — so the
//! parser reports a volume, its size, and its file count. But the
//! catalog B-tree pages, inode records, and file extents are all
//! FileVault-encrypted, and decryption happens in the *APFS software
//! driver*, above the block layer. Raw reads via `/dev/rdiskN` return
//! ciphertext for those objects, and Fletcher-64 checksum validation
//! fails immediately with `invalid checksum` on the first B-tree walk.
//!
//! Fixing this requires obtaining the Volume Encryption Key from the
//! system keybag via `SecKeychain` + user auth, then decrypting the
//! FileVault stream in userspace. Substantial engineering, not planned.
//!
//! **Live-volume caveats:**
//! - The APFS parser is currently a *skeleton* here. We surface the entry
//!   points and CLI flag now; the real path-resolution + extent read
//!   implementation will land as we vet the vendored crate against real
//!   macOS-generated volumes.
//! - For consistency on a mounted (actively-being-written) volume, we
//!   should target a snapshot created via `fs_snapshot_create`. On idle
//!   external drives (tzip's actual target) this is unnecessary — the
//!   latest committed checkpoint is stable across reads.

#![cfg(all(feature = "raw-apfs", target_os = "macos"))]

use anyhow::{anyhow, Context, Result};
use std::path::Path;

use crate::pipeline::Source;
use crate::platform::ReadBuf;
use crate::walker::WorkItem;

/// Source implementation backed by direct reads of `/dev/rdiskN`.
///
/// SKELETON — returns an unimplemented error today. Wiring is deliberate:
/// the CLI flag, feature gate, and pipeline plumbing exist so we can
/// iterate on the parser without touching everything else.
pub struct RawApfsSource {
    #[allow(dead_code)]
    device: std::path::PathBuf,
    // TODO: an `apfs::ApfsVolume` handle once we've opened the container
    // and located the target volume.
}

impl RawApfsSource {
    /// Open `/dev/rdiskN` for the given source mount and locate the APFS
    /// volume that backs it.
    ///
    /// Not implemented yet — currently returns an error explaining what's
    /// missing so `--raw-block` fails cleanly at startup instead of at
    /// first read.
    pub fn open_for_mount(mount_point: &Path) -> Result<Self> {
        // Steps once implemented:
        // 1. statfs(mount_point) → f_mntfromname = "/dev/diskNsM"
        // 2. Derive whole disk (/dev/diskN) and its character device (/dev/rdiskN)
        // 3. Open /dev/rdiskN as a File (requires root or `operator` group)
        // 4. Parse APFS container superblock via `apfs` crate
        // 5. Enumerate volumes; pick the one whose role/name matches mount_point
        // 6. Optionally: fs_snapshot_create + read the snapshot for consistency
        Err(anyhow!(
            "--raw-block: RawApfsSource for {} is not implemented yet. \
             Skeleton wired; parser integration is the next step. \
             Fall back to the default Source or omit --raw-block.",
            mount_point.display()
        ))
    }
}

impl Source for RawApfsSource {
    fn read(&self, _item: &WorkItem) -> Result<ReadBuf> {
        // Once open_for_mount is implemented:
        // 1. Look up item.name_in_archive within the APFS catalog (b-tree walk)
        // 2. Resolve the file inode's extent list
        // 3. `pread` each extent from the raw device into a Vec / mmap-of-anon
        // 4. If cmpfs-compressed, decompress via `apfs`'s cmpfs helper
        // 5. Return ReadBuf::Owned
        Err(anyhow!("RawApfsSource::read not implemented"))
            .with_context(|| "raw-apfs skeleton — see src/raw_apfs.rs")
    }
}
