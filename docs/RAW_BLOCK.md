# `--raw-block` — raw APFS block reader

**Status:** scaffolding shipped; real read path pending.
**Platform:** macOS only.
**Cargo feature:** `raw-apfs`.
**Build:** `cargo build --release --features raw-apfs`.

## What it does

Bypasses the VFS layer entirely by opening `/dev/rdiskN` and parsing
APFS on-disk format directly (via the vendored [`apfs`](../vendor/dpp/apfs)
crate, MIT). File `open()` never happens as far as the kernel's file
descriptor table is concerned — meaning on-access AV hooks
(Sophos/CrowdStrike/SentinelOne/etc.) that intercept `open()` via
Endpoint Security see nothing.

## Why we built it

On corporate-managed Macs with EDR + Sophos-class AV, per-file scan
overhead can drop tzip's throughput 5-10× from the drive's actual
capability. See [benchmarks in the README](../README.md#performance)
for concrete numbers. The exclusion route works when IT cooperates;
`--raw-block` is the fallback when it doesn't.

## Access requirements

`/dev/rdisk*` nodes are `root:operator 0640` — regular users can't
read them. Options:

| Approach | Effort | Blast radius |
|---|---|---|
| `sudo tzip …` per invocation | zero setup | minimal — one command |
| `sudo dseditgroup -o edit -a $USER -t user operator` | one-time + logout/login | persistent — any future shell you run |
| setuid tzip binary | not recommended | anyone on the box gets raw-disk access |

## What volumes are supported

**Works:**
- **Unencrypted external APFS drives** (typical tzip target — SanDisk
  Extreme V3, WD Elements, etc., formatted APFS via Disk Utility
  without turning on FileVault)
- **Unencrypted internal APFS volumes** (rare on Macs shipped with
  FileVault by default, but possible)
- **Sealed system volumes** — these are Merkle-tree-verified and
  structurally frozen. Reading works; not usually what you want to
  archive.

**Does not work: FileVault-encrypted volumes.**

## FileVault limitation — details

On a FileVault-enabled APFS volume, the encryption layout is:

- Container superblock (NX): **unencrypted** (must be readable to
  bootstrap decryption)
- Volume superblock (APSB): **unencrypted** (identifies the volume)
- Catalog B-tree pages: **FileVault-encrypted**
- Inode records: **FileVault-encrypted**
- File extent data: **FileVault-encrypted**

The decryption happens in the **APFS software driver in the kernel**,
above the block layer. When you mount the volume, macOS unlocks the
Volume Encryption Key (VEK) via the SEP and hands it to the APFS
driver. Reads *through* the mounted filesystem get plaintext. Reads
directly from `/dev/rdiskN` do **not** — the block layer returns the
on-disk ciphertext.

### Observed behavior

Running the probe against a mounted, unlocked FileVault Data volume:

```
$ sudo target/release/examples/raw_apfs_probe /dev/rdisk3
== raw_apfs_probe ==
device: /dev/rdisk3
open: OK in 0.42ms
APFS parse: OK in 0.02s

=== volume ===
  name:          Macintosh HD - Data
  block_size:    4096
  num_files:     7962045      ← matches macOS's own count
  num_dirs:      1116573
  num_symlinks:  165099
list_directory(/) failed: invalid checksum   ← FileVault ciphertext
```

Container + volume superblocks parse cleanly because they're
unencrypted; the first attempt to walk the catalog B-tree hits
FileVault-encrypted pages and Fletcher-64 rejects them as corrupt.

Deterministic across runs (5/5 identical output), so it's not a torn
read or racing checkpoint — it's structural: we're reading ciphertext.

### Why we're not fixing this

Fixing would require:

1. Enumerate volume's class keys from the keybag (stored in
   `nx_keybag_locker` / `apfs_keybag_locker`)
2. Retrieve the VEK by prompting the user for password / Touch ID via
   `authd` / `SecKeychain`
3. Implement AES-XTS decryption of block ranges in userspace using
   the VEK
4. Cover class-key hierarchies (per-user data protection, protected
   directories, etc.)

Roughly weeks of implementation plus a security audit — for a feature
that's a workaround for FileVault-on-internal-with-AV, a niche that
already has a supported alternative: mount the volume, `sudo tzip
--raw-block=false` (default path).

## Live volume caveats

On an actively-being-written volume, the "latest checkpoint" is a
moving target. We could read a checkpoint superblock, then when we
resolve an OMAP pointer downstream, the referenced page might have
been superseded. The vendored `apfs` crate today always picks the
latest checkpoint via `find_latest_nxsb`.

For tzip's typical use case (bulk archiving from an external drive
that isn't being written to), the latest checkpoint is stable and this
isn't an issue.

For actively-mounted volumes, the fix is APFS snapshots:

```
sudo tmutil localsnapshot          # creates a snapshot on the boot volume
# For an external drive, use `sudo fs_snapshot_create` (requires libc FFI)
```

Then extend the `apfs` crate to open at a specific snapshot's XID
instead of the latest checkpoint. Not implemented yet.

## AV / EDR alerts on raw disk access

Opening `/dev/rdiskN` is visible to Endpoint Security. Sophos,
CrowdStrike, SentinelOne, etc. all can (and often do) subscribe to
`ES_EVENT_TYPE_NOTIFY_OPEN` and log/alert on unusual processes
touching raw block devices. `tzip --raw-block` bypasses the *scanning*
hook (`ES_EVENT_TYPE_AUTH_OPEN`) but **cannot** hide from the
*notification* hook.

Expected consequence in enterprise environments: your IT admin gets a
Sophos Central event to review. It won't block the operation, but it
may prompt a conversation. If you're using `--raw-block` regularly, a
better long-term arrangement is asking IT for a Sophos
*process-based* exclusion for the `tzip` binary — that skips scanning
for files opened by tzip, without needing raw block access, and
doesn't generate the raw-disk-access alert.

## Testing

The example probe binary is a good end-to-end smoke test on a new
drive:

```
cargo build --example raw_apfs_probe --features raw-apfs --release
sudo target/release/examples/raw_apfs_probe /dev/rdiskN
```

Expected on an unencrypted external APFS drive:
```
open: OK
APFS parse: OK
=== volume ===
  name: <your drive name>
  ...
=== root directory (top 10 entries) ===
  Directory  .Spotlight-V100  0 bytes
  Directory  .fseventsd  0 bytes
  ...
```

If you see `list_directory failed: invalid checksum`, the volume is
FileVault-encrypted and `--raw-block` will not work on it.

## Roadmap

- [ ] Wire real `RawApfsSource::open_for_mount` (currently a skeleton
      that returns an unimplemented error and falls through to the
      default reader)
- [ ] Extend vendored `apfs` crate to accept a specific volume index
      in a container (instead of always the first)
- [ ] `fs_snapshot_create` integration for live-mounted volumes
- [ ] End-to-end benchmark vs the default `LocalFsSource` /
      `DispatchIoSource` paths
- [ ] FileVault VEK derivation (unlikely — see "Why we're not fixing
      this" above)
