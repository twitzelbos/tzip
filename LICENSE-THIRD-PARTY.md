# Third-party components vendored into tzip

tzip itself is BSD-3-Clause (see [LICENSE](LICENSE)). It vendors the
following third-party components with their own licenses. Attribution is
retained; using tzip does not automatically require you to comply with
these terms unless you enable the associated Cargo feature.

## `apfs` and related crates (from `Dil4rd/dpp`)

- **Vendored path**: `vendor/dpp/` (git submodule)
- **Upstream**: <https://github.com/Dil4rd/dpp>
- **License**: MIT (see `vendor/dpp/LICENSE`)
- **Copyright**: 2026 Dil4rd
- **Enabled by**: `--features raw-apfs`
- **Used from**: `src/raw_apfs.rs`
- **Sub-crates linked**: `apfs`, `cmpfs`

The default tzip build does not link these crates and is unaffected by
their license. Enabling the `raw-apfs` feature pulls the MIT-licensed
`apfs` and `cmpfs` crates into the resulting binary. MIT is compatible
with BSD-3-Clause; downstream users must retain both copyright notices
if redistributing a `--features raw-apfs` build.

## Standard Rust ecosystem crates

All crates listed under `[dependencies]` in `Cargo.toml` from crates.io
are their own licensors' terms (typically MIT/Apache-2.0). Run
`cargo license` in the repo to enumerate.
