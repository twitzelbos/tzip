# Cold storage: Glacier Deep Archive economics for small-file corpora

## Scenario

A healthcare research lab must retain anonymized study data for
**compliance**:

- **8 million small files** — per-patient CSVs, DICOM headers, study
  metadata JSONs.
- Median file ~12 KB, total ~96 GB logical.
- Access expectation: **almost never read**. Maybe 1-4 retrievals a year
  for audit. Retention: **7 years minimum** by regulation.
- Current plan: dump straight to S3 Glacier Deep Archive at
  ~$0.00099/GB-mo. Looks like **$0.10/mo**. Seems cheap.

Then the actual bill arrives and it's **$55/mo**. Why?

## Where the pain is

### Deep Archive's small-object minimum

The headline price is misleading for small objects. From the AWS pricing
page:

> "Deep Archive Access, minimum billable object size: **32 KB**."
> Additionally, metadata overhead of **40 KB** per object is billed at
> Standard rate.

For an 8-million-file corpus of 12 KB median:

| Component | Formula | Monthly |
|---|---|---|
| Billable data | 8M × max(12 KB, 32 KB) = 256 GB | 256 × $0.00099 = **$0.25** |
| Metadata overhead | 8M × 40 KB = 320 GB at Standard $0.023 | **$7.36** |
| Each object's minimum storage duration of 180 days ⇒ early deletions billed as 180-day object | (not applicable at steady state) | — |
| PUT (upload one-time) | 8M × $0.05/1k | $400 **one-time** |
| **Monthly storage** | | **~$7.61** |

Not $0.10. And that's **before** we talk about retrievals.

### Retrievals bite harder

If an audit needs 10,000 of those files:

- 10,000 restore API calls at Bulk tier: $0.025 / 1,000 = $0.25
- Storage (temporary): 10k × 12 KB = ~120 MB = negligible
- GET after restore: 10,000 × $0.0004/1000 = $0.004
- **Egress** (to lab laptop, cross-region): 120 MB × $0.09/GB = $0.01

That's only $0.26 per retrieval — cheap. But the **time**: Deep Archive
retrieval is **12 hours minimum** per object, Bulk tier. 10,000 objects
all restore in parallel, so wall-clock is bounded by the single longest
retrieve ≈ 12 h. If retrievals are serialized (naive scripting), you're
looking at days.

### Encryption + inventory

- SSE-KMS on each object adds ~$0.03/10k decrypts. 8M is noise for audit
  retrievals but significant for any kind of "scan all" check.
- S3 Inventory report (daily listing of what's actually in your Deep
  Archive bucket) scales with object count. Running daily inventory on
  8M objects produces a ~500 MB CSV each day. Billable.

### 7-year TCO

| Line | Loose Deep Archive | |
|---|---|---|
| Monthly storage | $7.61 | |
| One-time upload PUT | $400 | |
| 7-year storage (84 mo × $7.61) | $639 | |
| 7-year PUTs (updates, if any) | varies | |
| Retrieval cost (~4/yr × 7 yr = 28 retrievals, modest size) | ~$10 | |
| **Total 7-year TCO** | **~$1,049** | |

The storage headline was $0.10/mo; the real headline is $12/mo once you
account for the small-object penalty and metadata rate.

## The tzip take

Bundle into **a small number of archives** (one per study, or one per
year), upload those, and keep the per-object count low. Because tzip's
archives are first-class S3 objects, Range retrievals work normally on
them even in Standard-class — but at Deep Archive tier, you can't Range
GET anyway (whole object retrieval is required), so bundling further
collapses the "everything or nothing" granularity to match.

```
# One-time: package by study. Compression is irrelevant to Deep
# Archive price (storage is by actual size, 32 KB min per object),
# so pick aggressive compression for size and leave it.
tzip create study-A.zip /lab-data/study-A/ -m zstd -x 19 --write-sha256
tzip create study-B.zip /lab-data/study-B/ -m zstd -x 19 --write-sha256
# ... one archive per study ...

# Upload to Deep Archive.
aws s3 cp study-A.zip s3://cold-med-archive/ --storage-class DEEP_ARCHIVE

# Store the per-entry SHA-256 report once, so auditors can verify
# content integrity after any future retrieval.
tzip hash --algo sha256 --json study-A.zip > study-A.sha256.jsonl
aws s3 cp study-A.sha256.jsonl \
  s3://cold-med-archive-index/ --storage-class STANDARD_IA
```

### Scale comparison

Hypothetical: 50 studies, average 160k files each, average 2 GB logical
per study.

| Layout | Objects | Billable storage (Deep Archive) | Metadata ($0.023/GB) | Monthly |
|---|---|---|---|---|
| 8M loose files (12 KB avg) | 8,000,000 | 256 GB × $0.00099 = $0.25 | 320 GB × $0.023 = $7.36 | **$7.61** |
| 50 archives (2 GB avg, zstd-19 → ~1 GB each) | 50 | 50 GB × $0.00099 = $0.050 | 50 × 40 KB × $0.023 = $0.000046 | **$0.05** |

**Monthly storage drops 150× ($7.61 → $0.05).**

Over 7 years: **$639 vs $4** in storage alone, plus **$400 one-time PUT
cost drops to $0.0025** (50 PUTs vs 8M PUTs).

### Retrieval economics

Retrieving a loose Deep Archive object → the object comes back to
Standard class. 10,000 individual Bulk restores → one 12 h wait,
$0.25 in restore charges.

Retrieving a tzip archive → **one** restore call, 12 h wait,
**$0.000025** per archive. Once restored:

- The whole archive is accessible as a Standard-class object.
- `tzip ls`, `tzip grep`, `tzip x --to /audit/` all work normally.
- Selective extraction with Range GETs costs the normal Standard-class
  GET price — no additional restore charges per entry.

### Minimum storage duration

Deep Archive has a **180-day minimum storage duration**. Delete an
object after 90 days and you still pay for 180.

- Loose: 8M objects each subject to 180-day min individually. Any
  "oops, deleted a sensitive file" is a full 180-day stuck charge **per
  object touched**.
- tzip: one archive, one 180-day commitment. Fewer surfaces for
  accidental early-delete charges.

### `--write-sha256` + offline index

The `tzip hash --algo sha256 --json archive.zip > index.jsonl` sidecar
(stored in cheap Standard-IA) is tiny — one line per entry with name
+ hash. Total size for 160k-entry study ≈ 25 MB.

That sidecar lets auditors:

1. Confirm "the archive on disk matches the archive we stored" by
   comparing a single SHA-256.
2. Confirm per-file provenance by checking individual entries' hashes
   **without ever restoring the Deep Archive object** — because tzip
   writes the SHA-256 into the archive's extra field, and the sidecar
   mirrors it for offline reference.

### Shard size + retrieval parallelism

If a single study's archive gets too big for comfortable retrieval (say
> 100 GB), shard it:

- `tzip create study-A-part-01.zip ... --include 'patients/0000-1999/*'`
- `tzip create study-A-part-02.zip ... --include 'patients/2000-3999/*'`

A 12 h restore is the same wall-clock whether you restore one 100 GB
archive or ten 10 GB archives in parallel — Deep Archive restores are
per-object jobs. You pick a shard size that's a multiple of your
"restore granularity" need, not of your file count.

## Features at work

- `src/pipeline.rs` with `-m zstd -x 19` — highest-ratio compression
  for the one-time archive build.
- `src/hash.rs` + `src/zipreader.rs` extra field (PR #43) —
  `--write-sha256` bakes per-entry digests into the archive so audit
  integrity checks don't need to decompress bodies.
- `--sort` — byte-identical archive output for reproducible archival
  (useful when a regulator asks "is this the exact file you deposited
  5 years ago?").
- `src/sources/s3.rs` — once an archive is restored to Standard class,
  Range GETs + `tzip x` / `tzip grep` all work normally against the
  S3 object.

## When this DOESN'T fit

- **Small total corpora (< 1 GB).** The small-object penalty isn't big
  enough to matter.
- **Objects accessed one-at-a-time in Standard class.** Deep Archive
  requires full-object restore; if your workflow pulls individual
  files frequently, don't put them in Deep Archive regardless of
  bundling.
- **Regulator mandates per-file object-level immutability.** Some
  HIPAA / FDA audit frameworks want the archive layer to track each
  original file's upload time / modification history independently.
  Check your specific compliance requirements before bundling.

## Rough rule

> **Deep Archive + millions of small files is a trap.** The headline
> $0.00099/GB-mo price assumes objects well above the 32 KB billable
> minimum and ignores the 40 KB/object metadata-overhead-at-Standard
> rate. If your average file size is under ~100 KB and object count
> is in the millions, bundling into a few hundred tzip archives
> before cold-tiering typically reduces the monthly storage line
> by 50–150× while preserving selective retrieval via post-restore
> Range reads.
