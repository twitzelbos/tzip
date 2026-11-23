//! Stub — install / uninstall / mount dispatch lands next.

use anyhow::{bail, Result};

use super::MountOptions;

pub fn install() -> Result<()> {
    bail!("tzip mount --install: FSKit installer not yet implemented in this PR revision")
}

pub fn uninstall() -> Result<()> {
    bail!("tzip mount --uninstall: FSKit uninstaller not yet implemented in this PR revision")
}

pub fn mount(_opts: MountOptions) -> Result<()> {
    bail!("tzip mount on macOS via FSKit: mount dispatch not yet implemented in this PR revision")
}
