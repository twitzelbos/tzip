# CI/CD: build-artifact distribution for 50 engineers

## Scenario

A medium-sized engineering team ships a C++/Rust/CMake monorepo:

- **Per-commit CI builds produce ~2,400 artifacts**: compiled test binaries,
  static libs, debug symbols, generated Python bindings, HTML
  documentation, intermediate `.ll` IR for ablation studies.
- Total output per green build: **~3.4 GB raw, ~0.9 GB zstd level 3**.
- Backend runs ~80 builds a day during the workday, ~20 overnight on
  main.
- **50 engineers** periodically pull the latest green build's artifacts
  to debug locally — not every build, but every engineer pulls several
  per day on average.
- Current layout: each artifact uploaded as its own S3 object under
  `s3://ci-artifacts/<commit>/`.

## Where the pain is

### Upload is slow

Each CI runner has to upload 2,400 objects per build:

- Even with S3 SDK connection pooling (default 10) and `s3://` multipart:
  at 20 uploads/s sustained, **120 seconds per build** just on the
  upload step before the "mark green" webhook fires.
- The CI dashboard shows build-step latency; devs wait for green before
  rebasing. Upload dominates the last 2 minutes.

### Download is slower, and 50× over

Each engineer pulling a build via `aws s3 cp --recursive`:

- 2,400 objects × 30 ms per GET at p99 = **72 seconds minimum** latency
  independent of file size.
- Even with `--jobs 64` SDK parallelism it's ~20 seconds for the API
  overhead alone on a happy day.
- Multiplied across 50 engineers pulling 3 builds/day = 150 pulls/day =
  **~50 minutes of aggregate engineer-wait on API overhead alone**.

### S3 bill

At 100 builds/day × 2,400 objects × 30 days:

| Line item | Amount | Cost |
|---|---|---|
| PUT (uploads) | 7.2M/mo | $36.00 |
| GET (pulls, ~150 pulls/day × 2,400 objects × 30 days) | 10.8M/mo | $4.32 |
| Storage at 2% of objects being long-lived | ~5 TB | $115/mo |
| **Total** | | **~$155/mo** |

For a mid-sized team this is noise, but note the API cost skews toward
PUT (uploads).

### Random cold-cache pulls hurt most

The first engineer to pull a commit after it ages out of the S3-edge
cache (minutes-to-hours later) pays the full p99 latency on every one
of the 2,400 objects. Repeat-pullers get a faster path from caching,
but the first one always eats it.

### Partial pulls are hard

A common ask: _"I only need the x86_64 debug binaries, not the
docs/wheels/sanitizer builds."_

- With loose objects: the engineer has to `aws s3 ls` the prefix,
  filter names in the shell, then invoke `aws s3 cp` per file. Two
  rounds of API calls, hard to script.
- Everyone ends up pulling the whole set because scripting partial
  pulls is annoying.

## The tzip take

CI writes **one archive per build**, tagged by commit. Engineers pull via
`tzip x` and can grab subsets by glob.

```
# On the CI runner — after linking / packaging:
tzip create /tmp/artifacts.zip ./build-out/ \
  -m zstd -x 10 --sort --write-sha256

# Deterministic: `--sort` guarantees byte-identical archive across
# runners for the same input tree, so content-addressable upload keys
# work.
sha=$(sha256sum /tmp/artifacts.zip | awk '{print $1}')
aws s3 cp /tmp/artifacts.zip s3://ci-artifacts/$COMMIT_SHA.zip

# From an engineer's laptop — pull everything:
tzip x s3://ci-artifacts/$COMMIT_SHA.zip --to ./out/ -P 32

# Or just the x86_64 debug binaries:
tzip ls s3://ci-artifacts/$COMMIT_SHA.zip  # peek the entry names
tzip x s3://ci-artifacts/$COMMIT_SHA.zip \
  'bin/x86_64-debug/*' \
  --to ./out/ -P 32

# Diff two builds to see what changed on disk-footprint terms:
tzip diff \
  s3://ci-artifacts/abc123def456.zip \
  s3://ci-artifacts/def456abc789.zip
```

### Upload is one PUT

The CI runner builds one 900 MB archive and uploads one object. At typical
~50 MB/s CI runner bandwidth, that's ~18 seconds — plus a few seconds of
archive-build CPU, versus 120 seconds of API serial-PUT overhead for 2,400
loose objects.

**Upload time: 120 s → ~20 s per build.**

### Download is bandwidth-bound, not API-bound

Engineers pulling the archive see:

- One Range GET for the CDR (hundreds of KB)
- `tzip x -P N` fans out Range GETs for all entry bodies in parallel; the
  shared connection pool means you don't pay the 30 ms per-object latency
  as a serial sum
- Measured: on a 1 Gbit office line, ~900 MB arrives in ~8 seconds. On a
  home Wi-Fi link, bandwidth-bound at ~50 Mbit means ~2 min.

**Download time (full set): typical ~72 s API overhead eliminated.**
For a partial pull (`'bin/x86_64-debug/*'`), the API cost drops to just
the Range GETs for the matching entries, instead of the 2,400 object
GETs you'd need loose.

### Deterministic archives

`tzip create --sort` emits byte-identical output for the same input tree
(same file contents, same mtime, same mode). That means:

- **Content-addressable keys** (`s3://ci-artifacts/<sha256-of-archive>.zip`)
  work. If two runners rebuild the same commit, they produce the same
  archive, same S3 key → **free deduplication** at the S3 layer.
- **Signature verification** is just comparing hashes — no need to walk
  2,400 individual object ETags.

### Monthly cost breakdown

Same 100-builds/day workload:

| Line item | Loose | tzip-bundled |
|---|---|---|
| PUT (uploads: 100 builds × 2400 obj vs 100 builds × 1 obj) | $36/mo | **$0.015/mo** |
| GET (pulls: 150/day × 2400 vs 150/day × ~CDR + ~body batches) | $4.32/mo | **$0.045/mo** |
| Storage | $115/mo | **~$115/mo** (same data, minor compression on top of each tool's native format) |
| Engineer-wait time on downloads (150 pulls × ~60 s API saved) | ~2.5 h/day team-wide | ~none |
| **Monthly API bill** | **$40.32** | **$0.06** |

The engineer-wait time is the actual money: 2.5 h/day × 20 workdays =
50 engineer-hours/month spent watching progress bars.

### Deterministic builds + archive-level signing

`tzip --sort` is what makes "the exact same commit produces the exact
same build artifact bundle" actionable. Attach a GPG signature over the
bundle once per build; verification is one signature check, not 2,400.
`tzip hash --algo sha256 --write-sha256` at create time also stores each
entry's digest inside the archive, so individual-file provenance surveys
are body-free after the archive is built.

### Append-only build cache on top of a baseline

If 80% of a build's output is cache-identical across commits (static
libs, generated code, docs), use tzip's update mode:

```
# Start from the previous commit's archive, add what changed, remove
# what's gone. Byte-copies kept entries; only re-encodes modified ones.
cp cache/prev-commit.zip cache/this-commit.zip
tzip u cache/this-commit.zip \
  --delete 'bin/removed-*' \
  build-out/bin/new-and-changed/
```

Append mode avoids re-compressing entries that didn't change between
builds. On a typical incremental CI build where 5% of files change,
rebuild time goes from "re-compress 900 MB" to "re-compress 45 MB +
byte-copy the rest." tzip's byte-copy carry path (PR #36) and single-
open append path (PR #38) make this fast.

## Features at work

- `src/pipeline.rs` — the create path. Parallel walker + reader +
  compressor + writer, throughput ~330 MiB/s on a laptop.
- `src/update.rs` — append mode for incremental builds (and rebuild
  mode for safe atomic swaps).
- `--sort` flag — deterministic output ordering for content-addressable
  CI pipelines.
- `src/sources/s3.rs` — Range GETs, so engineer-side pulls are bandwidth-
  bound, not API-call-count-bound.
- `src/reader.rs` — parallel extract with `-P` fan-out (PR #30, #46).
- `src/hash.rs` — `--write-sha256` embeds per-entry SHA-256 so artifact
  attestation workflows don't need a second pass over the body.

## When this DOESN'T fit

- **Container-registry flow.** If your CI already produces OCI images
  and uses a registry with content-addressable layers, keep that —
  OCI already solves most of these issues (differently). tzip fits
  the "just files" shape, not "layered image spec."
- **Per-file access control inside the bundle.** If some artifacts
  (e.g., signed keys, customer data samples) need different IAM than
  others, split them into separate archives.
- **Stable published release artifacts.** For
  `release/v1.2.3/linux-x86_64.tar.gz` the convention is already
  "one tar per release"; tzip helps only if there are a lot of
  files inside, which releases rarely have.

## Rough rule

> CI-stage build outputs with **>500 artifacts per build** and **>10
> consumers/day** are a strong fit for tzip bundling. The combined
> upload-time + download-time savings tend to exceed the one-time cost
> of switching the artifact layout within the first week.
