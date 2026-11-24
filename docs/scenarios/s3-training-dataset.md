# S3: 500,000 training images — ZIP vs loose objects

## Scenario

An ML team has an image corpus for model training:

- **500,000 JPEGs**, median 180 KB, total ~90 GB raw
- Stored in `s3://lab-datasets/imagenet-custom/` as **one object per image**
- Pipeline downloads and iterates them nightly for experiments; some runs
  pick a 10% random subset, others walk the whole set.

The standard answer — "just put the files on S3, they're independent" — is
intuitive but costs dramatically more than it should at that count. Here's
what the real bill and real latency look like, and how `tzip` collapses it.

## Where the pain is

### API cost: GETs dominate

AWS S3 Standard pricing (us-east-1 at time of writing):

- GET: **$0.0004 per 1,000 requests**
- LIST (ListObjectsV2): **$0.005 per 1,000 requests**, returns up to 1,000 keys/page

For one full-dataset training pass over 500,000 loose objects:

| Operation | Calls | Cost |
|---|---|---|
| LIST (500 pages × 1,000 keys) | 500 | $0.0025 |
| GET each object | 500,000 | $0.20 |
| **Per-run total** | | **$0.20** |

Not a huge single run — but typical ML teams run 10–50 experiments a month,
with partial-dataset sampling that over-reads common entries:

- **30 runs/month × 500k GETs = 15M GETs/month = $6/month in GETs alone**
- Plus the LIST overhead on every "what's new" check from cron / Airflow /
  Prefect polling the prefix.

Random-subset runs (common for hyperparameter sweeps) often re-GET the same
object across experiments — no S3-side caching discount; every call is
billed.

### Latency: 500k round trips

S3 GET latency to an in-region EC2 instance is ~15–30 ms per call. Out-of-
region or over the internet: 60–200 ms.

| Concurrency | Wall-clock for 500k GETs (@ 30 ms) |
|---|---|
| 1 | 4 h 10 min |
| 32 | 7 min 48 s |
| 100 | 2 min 30 s |
| 500 | 30 s (rate-limited on most subnets) |

At 100 concurrent connections (the typical SDK-default pool size), a single
training-data refresh is ~2.5 minutes **before you touch a GPU**. For a
100-experiment sweep that's 4+ hours of just-S3-API time.

### Egress

- In-region EC2 → S3: $0
- Cross-region or out-of-AWS: $0.09/GB first 10 TB

For a 90 GB training set going to an out-of-region dev laptop or a collab
partner's AWS account: **$8.10 per full download**.

### Storage

500k objects × ~180 KB mean = ~90 GB at $0.023/GB-mo = **$2.07/mo**

### Small-object multiplier

S3 has no per-object minimum, but every object carries ~1 KB of metadata
overhead in your bill accounting (inventory, access logs, tagging). For
hundreds of thousands of tiny objects, inventory reports take minutes to
generate and bloat observability tooling.

## The tzip take

Bundle the corpus into **one zip** (or a small number of shards), upload
once, then access via HTTP Range requests from S3.

```
# One-time: build the archive with per-entry SHA-256s baked in
# (so later consumers can verify integrity without downloading bodies).
tzip create imagenet-custom.zip ./imagenet-custom/ \
  -m deflate -x 6 --write-sha256

aws s3 cp imagenet-custom.zip s3://lab-datasets/

# Nightly training: list + fetch a random subset without touching the
# 499,500 objects you DON'T want.
tzip ls s3://lab-datasets/imagenet-custom.zip
tzip x  s3://lab-datasets/imagenet-custom.zip \
  cat_0042.jpg dog_1133.jpg owl_9988.jpg \
  --to ./epoch_batch/ -P 32

# CI / dataset diff: body-free comparison of two snapshots.
tzip diff \
  s3://lab-datasets/imagenet-custom-v1.zip \
  s3://lab-datasets/imagenet-custom-v2.zip
```

### How the API cost shrinks

tzip's remote reader uses **HTTP Range requests** against one object. Every
operation becomes a tiny number of GETs over the SAME object:

| Operation | API calls (tzip) | API calls (loose) |
|---|---|---|
| `tzip ls` of full archive | 1 (CDR tail fetch) | 500 (LIST pages) |
| `tzip cat <one entry>` | 1-2 Range GETs | 1 GET |
| `tzip x` full extract | ~`-P` parallel Range GETs saturating bandwidth | 500,000 GETs |
| `tzip x` 100-entry subset | ~100 Range GETs | 100 GETs |
| `tzip grep PATTERN` across archives | 1 CDR + N body Range GETs for matches | 500,000 GETs + decode each |
| `tzip find --min-size 1M` | 1 CDR per archive, zero body I/O | 500k GETs |

### Monthly cost table

Same 30-runs/month workload:

| | Loose objects | ZIP via tzip | Savings |
|---|---|---|---|
| Storage | 90 GB × $0.023 = $2.07 | 45 GB × $0.023 = $1.03 | **50%** |
| GET requests (full-epoch runs) | 15M × $0.0004/1000 = $6.00 | ~30 × 64-way parallel = ~2k GETs = $0.0008 | **>7000×** |
| GET requests (subset runs, 10% × 10 runs) | 500k × $0.0004/1000 = $0.20 | 500k × 1 CDR = negligible | n/a |
| LIST requests | 500 × 30 × $0.005/1000 = $0.075 | 0 (CDR-only) | ∞ |
| **Monthly total** | **~$8.14** | **~$1.03** | **~87% cheaper** |

Scale up to the real enterprise case (10M objects, 50 teams, cross-region
consumers) and the savings column reads in thousands of dollars per month.

### Storage compression

ImageNet-style JPEGs barely compress (already DCT-compressed) — we get
maybe 1-3%. But the corpus *around* the images often compresses well:

- Metadata JSONs / COCO annotations: 70-80% off with `deflate` level 6
- Scientific data (CSV, HDF5 index headers, protein structures): 60-90%
- Logs / text datasets: 85-95%

For a **mixed** corpus (training JSONs + images + annotations), 20-40%
total reduction is routine.

### Latency

Same 500k-entry extract, in-region:

| Approach | Wall clock |
|---|---|
| Loose GETs, 100 concurrency | ~2.5 min API, +14 min data = **16.5 min** |
| `tzip x s3://.../ archive.zip -P 32` | **data bandwidth-bound** — the 90 GB download at ~500 MB/s shared S3 throughput = ~3 min |

tzip's `-P` fans out Range GETs for the body stream across a shared HTTP
connection pool, so you saturate your EC2 instance's network pipe without
queuing one GET per object.

### Streaming without full download

Not every workflow wants to download everything. Common modes:

- **Random sample during training**: pick 256 entries per step, Range-GET
  each one. tzip reads the CDR once (cached on the worker), then does
  256 parallel Range GETs per step. The 499,744 entries you don't want
  never touch the network.
- **Streamed scan**: `tzip grep '"species":"corvid"' s3://.../labels.zip`
  — reads CDR, finds all `.json` entries, Range-GETs each, decompresses in
  parallel workers, greps, emits matching lines. One archive, one logical
  object.
- **Integrity check without download**: `tzip hash --algo sha256
  s3://.../ archive.zip` — if the archive was created with
  `--write-sha256`, this reads **only** the CDR (one Range GET per archive)
  and emits every entry's SHA-256. The whole dataset's integrity can be
  verified in under a second with zero body bytes transferred.

## Features at work

- `src/sources/s3.rs` — HTTP Range GETs with hand-rolled SigV4
- `src/sources/http.rs` — plain Range GETs for any HTTP-served archive
- `src/reader.rs::open_source_for_with_hint` — the main thread fetches the
  CDR once; parallel workers skip the probe round-trip (PR #33)
- `src/reader.rs::run_extract` — `-P` workers with per-worker connection
- `src/hash.rs` — `--algo crc32` and `--algo sha256` (with
  `--write-sha256` extras) emit per-entry digests from the CDR alone
- `src/grep.rs` / `src/find.rs` — CDR-side filtering, body-free listing

## When this DOESN'T fit

- **Mutable data.** ZIPs are append-friendly but not optimal for random
  per-object writes. For heavy mutation → stay with loose objects or an
  S3 Table / Delta layer.
- **Need per-object IAM.** ZIP sees a single S3 object; IAM applies to the
  whole archive, not individual entries. For fine-grained access control
  keep loose objects (or shard by access class).
- **Already < 10,000 objects.** The GET-per-object pricing isn't bad
  enough to justify the bundling overhead.

## Rough rule

> If you have **>10k files** that are read in groups (training, backup,
> search, verify, diff) and rarely mutated individually, bundling to a
> `tzip`-created archive is cheaper on storage, dramatically cheaper on
> API calls, faster to scan, and preserves random-access. The breakeven
> is lower if your access pattern involves listing or grep-like scans.
