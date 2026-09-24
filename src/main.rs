use anyhow::Result;
use clap::Parser;

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

fn main() -> Result<()> {
    let args = cli::Args::parse();
    let opts = args.into_options()?;
    pipeline::run(opts)
}
