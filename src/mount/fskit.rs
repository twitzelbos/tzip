//! macOS FSKit mount dispatch.
//!
//! - `install` / `uninstall` drive `OSSystemExtensionManager` via
//!   `fskit_install`.
//! - `mount` stashes the password in Keychain (so the plaintext never
//!   appears in `ps` / `mount` output), builds the FSKit options
//!   string, shells out to `mount(8) -t tzip`, then blocks until
//!   SIGINT/SIGTERM. On signal: `umount` + Keychain cleanup.

use anyhow::{bail, Context, Result};
use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};
use std::process::Command;

use super::{fskit_install, keychain, MountOptions};

pub fn install() -> Result<()> {
    fskit_install::install()
}

pub fn uninstall() -> Result<()> {
    fskit_install::uninstall()
}

pub fn mount(opts: MountOptions) -> Result<()> {
    // 1. If a password was supplied, mint a Keychain tag and store the
    //    password under it. The mount options string carries the tag,
    //    not the password.
    let keychain_tag = if let Some(pw) = opts.password.as_deref() {
        let tag = keychain::fresh_tag();
        keychain::store(&tag, pw)
            .with_context(|| format!("store AES password in Keychain (tag {})", tag))?;
        Some(tag)
    } else {
        None
    };

    // 2. Build the mount options string. Format must match what
    //    `macos-fskit/Sources/TzipFSExtension/TzipFSExtension.swift`'s
    //    `parseMountOptions` consumes: comma-separated `key=value`
    //    pairs with `,` / `=` / `%` percent-encoded in values.
    let mut opt_parts: Vec<String> = Vec::new();
    opt_parts.push(format!("url={}", percent_encode(&opts.archive_spec)));
    opt_parts.push(format!("cache_mb={}", opts.cache_mb));
    if let Some(tag) = &keychain_tag {
        opt_parts.push(format!("keychain={}", tag));
    }
    if opts.allow_other {
        opt_parts.push("allow_other".to_string());
    }
    if opts.verbose {
        opt_parts.push("verbose".to_string());
    }
    // Writable FSKit mount: the Swift extension picks this up in
    // `parseMountOptions` and dispatches to `tzip_open_volume_rw`.
    // Remote archives are rejected on the Rust side (commit needs an
    // atomic rename on a real filesystem path).
    if opts.write {
        let is_remote = opts.archive_spec.starts_with("http://")
            || opts.archive_spec.starts_with("https://")
            || opts.archive_spec.starts_with("s3://");
        if is_remote {
            if !opts.quiet {
                eprintln!(
                    "tzip mount: --write ignored for remote archive; mount is read-only"
                );
            }
        } else {
            opt_parts.push("write".to_string());
        }
    }
    let opts_str = opt_parts.join(",");

    let mp_str = opts
        .mountpoint
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("mountpoint is not valid UTF-8"))?;

    if !opts.quiet {
        let mode = if opts_str.contains("write") { "rw" } else { "ro" };
        eprintln!(
            "tzip mount: {} \u{2192} {} ({})",
            opts.archive_spec,
            opts.mountpoint.display(),
            mode,
        );
        if mode == "rw" {
            eprintln!(
                "tzip mount: --write mode — pending changes commit on unmount via FSKit's umount path"
            );
        }
        if keychain_tag.is_some() {
            eprintln!("tzip mount: AES password stashed in Keychain for the mount's lifetime");
        }
    }

    // 3. Shell out to mount(8). "-F" skips fstab lookup; the device
    //    source is "none" per the convention for sourceless file
    //    systems (tzip reads from a URL carried in the options).
    let status = Command::new("mount")
        .args(["-F", "-t", "tzip", "-o", &opts_str, "none", mp_str])
        .status()
        .with_context(|| "spawn mount(8)")?;
    if !status.success() {
        // Clean up the Keychain entry if we stored one.
        if let Some(tag) = &keychain_tag {
            let _ = keychain::delete(tag);
        }
        bail!(
            "mount -t tzip failed (exit status {}). Is the tzip FSKit \
             extension installed and approved? Run `tzip mount --install` \
             first.",
            status
        );
    }

    if !opts.quiet {
        eprintln!(
            "tzip mount: mounted. Ctrl-C or `umount {}` to unmount.",
            opts.mountpoint.display()
        );
    }

    // 4. Block on SIGINT/SIGTERM. On signal, umount + Keychain cleanup.
    let mut signals = Signals::new([SIGINT, SIGTERM])
        .context("register signal handler")?;
    for sig in signals.forever() {
        if !opts.quiet {
            eprintln!("tzip mount: received signal {}, unmounting", sig);
        }
        let _ = Command::new("umount").arg(mp_str).status();
        break;
    }

    if let Some(tag) = &keychain_tag {
        let _ = keychain::delete(tag);
    }
    if !opts.quiet {
        eprintln!("tzip mount: unmounted");
    }
    Ok(())
}

/// Percent-encode characters that would break the FSKit mount options
/// parser or confuse the mount(8) command. Keeps standard URL chars
/// (`://`, `?`, `&`, `.`, `-`, `_`, `~`) readable; escapes `,`, `=`,
/// `%`, and anything non-printable.
fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() + 8);
    for &b in s.as_bytes() {
        let needs_escape = matches!(b, b',' | b'=' | b'%' | b' ' | b'\t' | b'\n' | b'\r')
            || b < 0x20
            || b >= 0x7f;
        if needs_escape {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        } else {
            out.push(b as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_url_chars() {
        assert_eq!(percent_encode("https://host/key.zip"), "https://host/key.zip");
        assert_eq!(percent_encode("s3://bucket/path/key.zip"), "s3://bucket/path/key.zip");
    }

    #[test]
    fn percent_encode_escapes_options_separators() {
        assert_eq!(percent_encode("a,b=c"), "a%2Cb%3Dc");
        assert_eq!(percent_encode("50%"), "50%25");
    }

    #[test]
    fn percent_encode_escapes_whitespace() {
        assert_eq!(percent_encode("hello world"), "hello%20world");
    }
}
