//! macOS FSKit mount dispatch. Install / uninstall delegate to
//! `fskit_install` (SystemExtensions.framework); mount proper lands
//! in PR #13.

use anyhow::{bail, Result};

use super::{fskit_install, MountOptions};

pub fn install() -> Result<()> {
    fskit_install::install()
}

pub fn uninstall() -> Result<()> {
    fskit_install::uninstall()
}

pub fn mount(_opts: MountOptions) -> Result<()> {
    bail!(
        "tzip mount on macOS via FSKit: the mount(8) dispatch is the last \
         remaining step (PR #13). The extension can be installed with \
         `tzip mount --install` now that PR #12 is in."
    )
}
