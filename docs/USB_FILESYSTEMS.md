# Choosing a filesystem for external USB on Linux (with tzip)

Which Linux filesystem gives the best performance on an external USB drive
depends on two things tzip cares about: whether the filesystem supports
**FIEMAP** (so the `--raw-block` extent-order reader can engage) and whether
the media is a **spinning HDD** (seek-bound) or a **USB SSD / flash** device
(bandwidth- and queue-bound).

**Short answer:** use **ext4** as the default, or **xfs** if large files
dominate. Avoid btrfs for raw throughput, and avoid exFAT/NTFS except when the
drive must also mount on macOS/Windows.

## Why it depends on FIEMAP + media type

- tzip's `--raw-block` reader uses the `FS_IOC_FIEMAP` ioctl to read files in
  physical-disk order (a forward sweep instead of a seek storm). FIEMAP is
  supported on **ext4, xfs, btrfs, f2fs** — but not meaningfully on
  exFAT/NTFS, so `--raw-block` is a no-op there.
- `--io-uring` is filesystem-agnostic, but it only pays off when the USB
  transport can queue commands (UASP — see below).
- Spinning HDDs are dominated by seek latency (the extent sweep is the big
  win); USB SSD/flash have no seek penalty and instead benefit from queue
  depth and raw bandwidth.

## Ranking

| Filesystem | Verdict for USB | Notes |
|---|---|---|
| **ext4** | **Best default** | Full FIEMAP, extent-based, low CPU overhead, strong random + sequential. The extent-order sweep shines. Handles both HDD and SSD well. |
| **xfs** | **Best for big files / fast USB SSD** | Full FIEMAP, excellent parallel + large-file throughput — pair with `--io-uring` and higher `--read-jobs`. Slightly heavier for millions of tiny files; cannot shrink. |
| **f2fs** | Good *only* on genuine flash | Log-structured, flash-optimized; FIEMAP works. The wrong choice for a spinning USB HDD (its design assumes flash). |
| **btrfs** | Avoid for raw speed | FIEMAP works, but copy-on-write fragments files over time (scatters extents → undermines the disk-order sweep) and data checksums add CPU. Great for snapshots/integrity, not throughput. |
| **exFAT / NTFS** | Interop only | No useful FIEMAP → `--raw-block` won't engage, so you lose the main Linux acceleration. Use only if the drive must also mount on Windows/macOS. |

## Concrete recommendations

- **Linux-only USB, spinning HDD → ext4.** Let tzip auto-tune: it detects
  removable media, enables `--raw-block`, and drops to `--read-jobs 1` for a
  single clean forward sweep — exactly right for seek-bound media.
- **Linux-only USB, SSD / flash → ext4** (safe) or **xfs** (if large files).
  Override the HDD default, since flash has no seek penalty and likes queue
  depth:

  ```
  tzip out.zip /mnt/usb/src --raw-block --io-uring --read-jobs 4   # or 8
  ```

- **Must share with macOS / Windows → exFAT**, and accept that `--raw-block`
  won't engage (io_uring still helps a little).

## The transport matters as much as the filesystem

- Prefer a **UASP** enclosure (SCSI command queuing) over plain USB Mass
  Storage (BOT). UASP is what lets `--io-uring` / parallel reads actually pay
  off. Check with `lsusb -t` — you want the `uas` driver, not `usb-storage`.
- Mount with **`noatime`** so the archive read pass doesn't generate metadata
  writes.
- Format aligned to 4 KiB, and use a USB 3.x+ port/cable.

## Note on `--apfs-device`

If you're archiving a Mac-formatted **APFS** drive with `tzip --apfs-device`,
the Linux filesystem question is moot: tzip reads the raw block device and
parses APFS itself, so the disk-order win comes from the parser's extent
handling and needs no Linux APFS driver or FIEMAP support. See
[`LINUX.md`](LINUX.md).
