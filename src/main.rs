use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

mod cli;
mod compress;
mod crypto;
mod pipeline;
mod platform;
mod progress;
mod sevenz;
mod tui;
mod walker;
mod zipwriter;

#[cfg(target_os = "macos")]
mod bulk_walker;

#[cfg(target_os = "macos")]
mod dispatch_io;

#[cfg(target_os = "macos")]
mod usb_info;

#[cfg(target_os = "macos")]
mod contention;

#[cfg(all(feature = "raw-apfs", target_os = "macos"))]
mod raw_apfs;

fn main() -> Result<()> {
    // Parse via ArgMatches so we can inspect which flags came from the
    // command line vs took the default (needed by the auto-tune step to
    // avoid overriding user intent).
    let matches = cli::Args::command().get_matches();
    let args = cli::Args::from_arg_matches(&matches)?;
    let user_flags = cli::UserSetFlags::from_matches(&matches);
    let opts = args.into_options(user_flags)?;
    pipeline::run(opts)
}
