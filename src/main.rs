//! tzip binary entry point. All real logic lives in `tzip::*` (see
//! `src/lib.rs`); this file only does argv parsing + dispatch so the
//! same library can be reused by the `libtzip.dylib` cdylib target
//! that the macOS FSKit extension links against.

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

use tzip::{aggregate, bench, cli, diff, find, grep, hash, jq, mount, pipeline, query, reader, tail, update, update_s3};

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
            let opts = args.into_options(uf)?;
            // `tzip u` on a non-existent archive falls through to
            // create (matches `7z u`); the modify path is only
            // reached when the file already exists. Dispatching
            // at argv-parse time keeps `pipeline` from depending
            // on `update`, breaking a long-standing module cycle.
            //
            // `s3://` archives route to `update_s3::run_update_s3`
            // (issue #68) — append via S3 multipart upload with
            // server-side part copy. The live object never enters
            // a half-written state: `CompleteMultipartUpload` is
            // the atomic swap, `If-Match` provides the CAS.
            if update_s3::is_s3_archive(&opts.archive) {
                update_s3::run_update_s3(opts)
            } else if opts.archive.exists() {
                update::run_update(opts)
            } else {
                pipeline::run(opts)
            }
        }
        cli::Cmd::Ls(args) => reader::run_ls(args.into_options()?),
        cli::Cmd::Cat(args) => reader::run_cat(args.into_options()?),
        cli::Cmd::X(args) => reader::run_extract(args.into_options()?),
        cli::Cmd::Grep(args) => grep::run(args.into_options()?),
        cli::Cmd::Mount(args) => mount::dispatch(args.into_action()?),
        cli::Cmd::Find(args) => find::run(args.into_options()?),
        cli::Cmd::Tail(args) => tail::run(args.into_options()?),
        cli::Cmd::Bench(args) => bench::run(args.into_options()?),
        cli::Cmd::Diff(args) => {
            // `tzip diff` carries a shell exit status (0 = identical,
            // 1 = differed) in its Ok value. Short-circuit the normal
            // "exit 0 on Ok" behaviour so pipelines can test it.
            let code = diff::run(args.into_options()?)?;
            std::process::exit(code);
        }
        cli::Cmd::Hash(args) => hash::run(args),
        cli::Cmd::Aggregate(args) => aggregate::run(args.into_options()?),
        cli::Cmd::Query(args) => query::run(args.into_options()?),
        cli::Cmd::Jq(args) => jq::run(args.into_options()?),
    }
}

fn subcommand_user_flags(top: &clap::ArgMatches, name: &str) -> cli::UserSetFlags {
    match top.subcommand_matches(name) {
        Some(m) => cli::UserSetFlags::from_matches(m),
        None => cli::UserSetFlags::default(),
    }
}
