//! `tzip mount --install` / `--uninstall` — drives
//! `OSSystemExtensionManager` via a tiny Obj-C shim
//! (`fskit_install.m`) linked in by `build.rs` behind the `fskit`
//! feature.
//!
//! The Obj-C side handles the delegate pattern (`requestNeedsUserApproval`,
//! `didFinishWithResult`, `didFailWithError`) and signals a semaphore;
//! we expose two blocking `extern "C"` entry points. This is
//! substantially less fiddly than wiring `declare_class!` on the Rust
//! side via `objc2` + `objc2-system-extensions`.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

/// Bundle identifier declared in `macos-fskit/Sources/Resources/Info.plist`.
const BUNDLE_ID: &str = "io.qbio.tzip.fskit";

extern "C" {
    fn tzip_sysext_activate(bundle_id: *const c_char, err_out: *mut *mut c_char) -> i32;
    fn tzip_sysext_deactivate(bundle_id: *const c_char, err_out: *mut *mut c_char) -> i32;
    fn tzip_sysext_free_error(err: *mut c_char);
}

pub fn install() -> Result<()> {
    let bundle = find_bundle().context("locate tzip.fs bundle")?;
    eprintln!("tzip: activating system extension from {}", bundle.display());
    eprintln!("tzip: (bundle identifier: {})", BUNDLE_ID);
    submit(Op::Activate)?;
    eprintln!("tzip: extension activated. Confirm with `sysextensionsctl list`.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    eprintln!("tzip: deactivating system extension ({})", BUNDLE_ID);
    submit(Op::Deactivate)?;
    eprintln!("tzip: extension deactivated.");
    Ok(())
}

enum Op {
    Activate,
    Deactivate,
}

fn submit(op: Op) -> Result<()> {
    let bid = CString::new(BUNDLE_ID).expect("static bundle id is NUL-free");
    let mut err_ptr: *mut c_char = ptr::null_mut();
    let rc = unsafe {
        match op {
            Op::Activate => tzip_sysext_activate(bid.as_ptr(), &mut err_ptr),
            Op::Deactivate => tzip_sysext_deactivate(bid.as_ptr(), &mut err_ptr),
        }
    };
    if rc == 0 {
        return Ok(());
    }
    let msg = unsafe {
        if err_ptr.is_null() {
            "unknown".to_string()
        } else {
            let s = CStr::from_ptr(err_ptr).to_string_lossy().into_owned();
            tzip_sysext_free_error(err_ptr);
            s
        }
    };
    bail!("SystemExtensions: {}", msg)
}

/// Find the `tzip.fs` bundle to activate. For activation specifically,
/// the OS needs to see the bundle on disk *and* under one of the
/// system's accepted locations (typically alongside the host app).
/// For a CLI like tzip, the convention is to drop the bundle next to
/// the binary at install time.
///
/// Search order:
/// 1. `$TZIP_FS_BUNDLE` env var override (absolute path).
/// 2. `<dir of argv[0]>/tzip.fs` (the `scripts/build-fskit-bundle.sh`
///    output in a development checkout).
/// 3. `<dir of argv[0]>/../Library/Extensions/tzip.fs` (Homebrew-ish
///    install layout).
fn find_bundle() -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var("TZIP_FS_BUNDLE") {
        let p = PathBuf::from(&override_path);
        if !p.exists() {
            bail!("TZIP_FS_BUNDLE={} does not exist", override_path);
        }
        return Ok(p);
    }

    let argv0 = std::env::current_exe().context("resolve argv[0]")?;
    let bin_dir = argv0
        .parent()
        .ok_or_else(|| anyhow!("argv[0] has no parent directory: {}", argv0.display()))?;

    let candidate = bin_dir.join("tzip.fs");
    if candidate.exists() {
        return Ok(candidate);
    }

    let hb_candidate = bin_dir
        .parent()
        .map(|p| p.join("Library/Extensions/tzip.fs"));
    if let Some(p) = hb_candidate {
        if p.exists() {
            return Ok(p);
        }
    }

    bail!(
        "cannot find tzip.fs bundle. Build it with \
         `scripts/build-fskit-bundle.sh` (deposits at target/release/tzip.fs), \
         then re-run from target/release/ or set TZIP_FS_BUNDLE=/path/to/tzip.fs"
    );
}

// Silence unused-import on the Path type when the above helpers get cfg'd out.
#[allow(dead_code)]
fn _touch_path() -> &'static Path {
    Path::new("")
}
