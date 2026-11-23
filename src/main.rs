//! tzip binary entry point. All real logic lives in `tzip::*` (see
//! `src/lib.rs`); this file only does argv parsing + dispatch so the
//! same library can be reused by the `libtzip.dylib` cdylib target
//! that the macOS FSKit extension links against.

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

use tzip::{cli, find, grep, mount, pipeline, reader};

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
        cli::Cmd::Mount(args) => mount::dispatch(args.into_action()?),
        cli::Cmd::Find(args) => find::run(args.into_options()?),
    }
}

fn subcommand_user_flags(top: &clap::ArgMatches, name: &str) -> cli::UserSetFlags {
    match top.subcommand_matches(name) {
        Some(m) => cli::UserSetFlags::from_matches(m),
        None => cli::UserSetFlags::default(),
    }
}
