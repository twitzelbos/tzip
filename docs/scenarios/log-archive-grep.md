# Logs: grep across a year of daily archives without rehydrating

## Scenario

A service-operations team stores application logs as daily rolled files:

- **~400 JSON log files per day** per service (one per hour per pod)
- **18 services**, 365 days per year → **~2.6 million files per year**
- Median log file: ~40 MB uncompressed, ~4 MB after gzip
- Retention: 2 years of history for compliance, 90 days hot, rest cold
- Common ops question: _"when was the last time this exception appeared?"_,
  _"did any request to endpoint X in the last 6 months return 500?"_,
  _"which services logged this correlation ID?"_

The team's current workflow:

1. Logs land in a log-aggregator (CloudWatch / Datadog / an Elastic cluster).
2. After 30 days they're moved to S3 as `.log.gz` files, one per hour per pod.
3. Queries older than 30 days require rehydrating from S3 into the
   aggregator — minutes of wait per query, expensive to run, and limited to
   one person at a time by the rehydrate quota.

## Where the pain is

### Query cost on the aggregator

Datadog Logs rehydration pricing (typical mid-tier contract):

- **$1.70 per million events rehydrated** (lowest tier)
- A medium-traffic service generates ~50M events/day
- Rehydrating 7 days to answer one forensics question → **$595 per query**

At 10 queries per month across the on-call team that's $6,000 just in
rehydration, before analyst time.

### Query cost on S3 directly

Grep over S3 the "right" way is Athena / S3 Select:

- **Athena**: $5 per TB scanned. Scanning 7 days × 18 services × 24 h × 40 MB
  = 120 GB **compressed on S3**. Decompressed and fed through Athena,
  that's ~1.2 TB scanned per query = **$6 per query**.
- Add S3 GET cost: 7 × 18 × 24 × 400 = 1.2M GETs at $0.0004/1000 = $0.48.

Cheaper than rehydration, but Athena spins up / times out / can't return
line context around a regex hit.

### Naive grep over the raw S3 dump

```
aws s3 cp s3://logs/2026-05-14/ /tmp/day/ --recursive
grep -r 'OutOfMemoryError' /tmp/day/
```

That's **86,400 GETs per day queried** (400 files × 18 services × 12 hours of
a working query window) plus the download time and local disk pressure.
Nobody actually does this.

### Latency

Rehydration windows take 10–45 minutes before a query is runnable.
Athena cold-start is 3–8 seconds; add minutes if the data was tiered to
Glacier. The on-call engineer is stuck waiting while the incident is live.

## The tzip take

Daily cold-tier log bundle, one archive per service per day. Query via
`tzip grep` directly against S3, body-free scan over the CDR plus Range
GETs for the subset that hits.

```
# Roll yesterday's logs into one archive, upload once. Cron / batch job.
tzip create /tmp/logs-2026-05-14-billing.zip \
  /mnt/logs/billing/2026-05-14/ \
  -m deflate -x 9 \
  --write-sha256

aws s3 cp /tmp/logs-2026-05-14-billing.zip \
  s3://cold-logs/billing/2026-05-14.zip --storage-class INTELLIGENT_TIERING

# Later, from ops laptop or on-call shell — grep ONE archive:
tzip grep 'OutOfMemoryError' s3://cold-logs/billing/2026-05-14.zip

# ... or grep across ALL archives for one service for a date range:
tzip grep 'correlation_id=5ff.*503' 's3://cold-logs/billing/2026-05-*.zip'

# ... or across every service for last month (bucket-glob fan-out):
tzip grep -P 32 '"error_code":"PAYMENT_DECLINED"' \
  's3://cold-logs/*/2026-10-*.zip'
```

### How grep over S3 actually works in tzip

Per archive:

1. **One Range GET** to fetch the CDR tail (~hundreds of KB to a few MB).
2. Per entry matching the filename glob (`--include`), one Range GET to
   stream the compressed body. Decompress per entry in a worker pool.
3. Matches stream out as `<url>:<entry>:<line_number>:<text>` as they're
   found — no need to wait for the whole scan.

On a bucket-glob query, tzip's `--parallel` (outer archive pool) and
`--inner-jobs` (per-archive worker pool) saturate the network pipe while
regex matching runs on CPU.

### Cost comparison

Scenario: one typical ops query — "find all `OutOfMemoryError` in the last
90 days of billing service logs."

**Loose .log.gz on S3:**

| | Calls | Cost |
|---|---|---|
| LIST (90 days × ~2 pages) | 180 | $0.0009 |
| GET every hour-file (18 svc × 24 h × 90 days) | 38,880 | $0.016 |
| Egress to ops laptop (cross-region) at 4 MB each | 155 GB | $13.95 |
| **Total** | | **$13.97** |

**tzip grep over bundled archives on S3:**

| | Calls | Cost |
|---|---|---|
| LIST (`s3://cold-logs/billing/*.zip`) | 1 page | $0.000005 |
| CDR fetch per archive (90 archives × ~1 CDR Range GET) | 90 | $0.000036 |
| Body Range GETs for entries whose name matches `--include '*.log'` (all) | 32,400 | $0.013 |
| Egress (only matched bodies if `--files-only`, otherwise full body stream) | ~155 GB | $13.95 |
| **Total** | | **~$13.96** |

For a FULL scan the egress dominates — tzip doesn't save egress once you
actually need every byte. But once you narrow — say,
`--include "billing-pod-A-*"` restricts to 40% of entries, or
`--since 2026-04-01 --until 2026-04-07` restricts to one week — the GET
and byte counts drop proportionally, **without** needing the Athena /
rehydration intermediary.

Where tzip clearly wins:

- **Incident triage**: a 7-day scan runs in under a minute of wall-clock
  (one engineer, no quota). Rehydration would take 10–45 min.
- **"Did this ever happen?"**: scan over 2 years of archives for one
  regex without instrumenting anything.
- **Correlation across services**: `tzip grep -P 64 '<req-id>'
  's3://cold-logs/*/2026-11-*.zip'` reads all services' CDRs in parallel,
  only pulls bodies from archives that actually have the ID (via the
  include-filename side-filter if the ID appears in log filenames; the
  false-positive cost is just a compressed GET per archive).

### Archive size + compression

Deflate at level 9 on structured log JSON: **~10:1 ratio** is routine
(repetitive field names, time stamps, hostnames). The compressed daily
bundle for billing service is ~400 × 4 MB = 1.6 GB loose vs ~450 MB
bundled. Annual bundle set (18 services × 365): ~2.9 TB loose, ~800 GB
bundled.

Storage math at Intelligent-Tiering Infrequent Access ($0.0125/GB-mo
after 30 days):

- Loose: 2.9 TB × $0.0125 = **$37/mo**
- Bundled: 800 GB × $0.0125 = **$10/mo**

### Checkpointed resumable scans

Long grep over an entire year? Interrupted on hour 23? tzip's checkpoint
file picks up where it left off:

```
tzip grep -P 32 \
  --checkpoint /tmp/outage-nov14.ckpt \
  'StackOverflowError' \
  's3://cold-logs/*/2026-*.zip'
# Ctrl-C mid-scan, come back later with --resume:
tzip grep -P 32 \
  --checkpoint /tmp/outage-nov14.ckpt \
  --resume \
  'StackOverflowError' \
  's3://cold-logs/*/2026-*.zip'
```

Already-processed `(archive, entry)` pairs are skipped on the second run.
Great for very long-running scans and for "I accidentally terminated my
SSH session."

## Features at work

- `src/grep.rs` — parallel scan, outer `-P` across archives, inner
  `--inner-jobs` per archive. S3-bucket glob supported.
- `src/sources/s3.rs` + `src/sources/http.rs` — Range GETs per entry body
- `src/reader.rs::hint_from_archive` (PR #33) — main-thread CDR probe
  shared with inner workers, zero re-probe round-trips.
- `--files-only` — print archive paths only (skip body streaming).
- `--checkpoint` / `--resume` — restartable scans.
- `src/find.rs` — same CDR-side filters (globs, size, mtime) but
  body-free; useful for "which archives even contain a matching entry"
  without reading any bodies.

## When this DOESN'T fit

- **Hot (< 24 h) logs where you want millisecond response.** Keep those
  in the live aggregator; bundle only after rotation.
- **Full-text indexing requirements.** tzip grep is linear-scan (regex +
  context lines). If you need "give me everything that mentions X in
  < 50 ms" you want Elastic / Loki / Grafana / etc. tzip is for
  forensics and compliance scans, not for real-time dashboards.
- **PII / GDPR deletion requirements per-entry.** Bundled archives are
  harder to redact in place. For right-to-be-forgotten compliance,
  prefer object-level encryption + delete, and bundle only
  anonymized logs.

## Rough rule

> If your log retention policy already involves "ship to S3 and rarely
> touch", bundling the daily per-service rollups into tzip archives
> turns _"rehydrate, pay $500, wait 45 minutes"_ into _"tzip grep, 60
> seconds, zero extra infrastructure"_. The compression savings pay for
> themselves.
