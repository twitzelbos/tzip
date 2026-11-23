use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

mod cli;
mod compress;
mod crypto;
mod decompress;
mod grep;
mod mount;
mod pipeline;
mod platform;
mod progress;
mod reader;
mod sevenz;
mod sources;
mod tui;
mod update;
mod walker;
mod zipreader;
mod zipwriter;

#[cfg(target_os = "macos")]
mod bulk_walker;

#[cfg(target_os = "macos")]
mod dispatch_io;

#[cfg(target_os = "macos")]
mod usb_info;

#[cfg(target_os = "macos")]
mod contention;

#[cfg(all(feature = "raw-apfs", any(target_os = "macos", target_os = "linux")))]
mod raw_apfs;

#[cfg(target_os = "linux")]
mod fiemap;

#[cfg(target_os = "linux")]
mod linux_raw;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
mod io_uring_src;

fn main() -> Result<()> {
    // Parse via ArgMatches so UserSetFlags::from_matches can distinguish
    // CLI-supplied flags from clap defaults — needed by the auto-tune step.
    let matches = cli::Cli::command().get_matches();
    let cli = cli::Cli::from_arg_matches(&matches)?;
    match cli.command {
        cli::Cmd::Create(args) => {
            let uf = subcommand_user_flags(&matches, "create");
            pipeline::run(args.into_options(uf)?)
        }
        cli::Cmd::U(args) => {
            let uf = subcommand_user_flags(&matches, "u");
            pipeline::run(args.into_options(uf)?)
        }
        cli::Cmd::Ls(args) => reader::run_ls(args.into_options()?),
        cli::Cmd::Cat(args) => reader::run_cat(args.into_options()?),
        cli::Cmd::X(args) => reader::run_extract(args.into_options()?),
        cli::Cmd::Grep(args) => grep::run(args.into_options()?),
        cli::Cmd::Mount(args) => mount::mount(args.into_options()?),
    }
}

fn subcommand_user_flags(top: &clap::ArgMatches, name: &str) -> cli::UserSetFlags {
    match top.subcommand_matches(name) {
        Some(m) => cli::UserSetFlags::from_matches(m),
        None => cli::UserSetFlags::default(),
    }
}
