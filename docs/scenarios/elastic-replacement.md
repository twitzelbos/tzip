# The archive IS the database: six scenarios enabled by tzip Phases 1–7 + 5b

Phases 1 through 7 of the Elastic feature-parity roadmap closed each row of
the "Elastic vs tzip" comparison table. Phase 5b made it possible to *keep
appending* to a columnar archive without ever rebuilding. Taken together,
these primitives unlock a category of use case: **long-retention, deep-query
analytics on single-file archives, with no cluster to operate and no loss of
SQL expressiveness.**

This document walks six concrete scenarios. For each: the pain today, the
tzip commands that solve it, the time and money saved — backed by published
cloud pricing or real-world numbers.

The scenarios share a baseline: your JSON records are already segmentable
by time, tenant, shard, or build. Everything below rests on tzip 0.2 +
system `duckdb` on PATH.

---

## 1. Seven-year audit log retention for SOC 2 / PCI-DSS / HIPAA

**Pain:** Compliance says "retain structured audit logs for 7 years and be
able to answer auditor questions in days, not weeks." Elasticsearch at 10 TB
retention is a six- or seven-figure annual bill (hot + warm + cold tiers,
plus the ops to keep them reliable). S3 + Athena works but needs a schema
in Glue, a CloudFormation stack, and $5 per TB scanned per query. Loose
JSON on S3 is cheap but un-queryable without rehydrating.

**With tzip:** emit one archive per day (or per hour at peak), each with a
columnar sidecar. Store on S3 Standard-IA or Glacier Instant Retrieval.

```bash
# Nightly, from the ingest side:
tzip create "audit-$(date +%F).zip" /var/log/audit/*.jsonl \
    --index-terms '*.jsonl' \
    --write-columnar schema.json

aws s3 cp "audit-$(date +%F).zip" s3://compliance-audit/

# Auditor asks: every admin API call from eu-west in Q2 2025 that returned 5xx
tzip query -s "
    SELECT ts, principal, path, status_code, remote_ip
    FROM tzip
    WHERE path LIKE '/admin/%'
      AND region = 'eu-west'
      AND status_code BETWEEN 500 AND 599
      AND ts BETWEEN '2025-04-01' AND '2025-06-30'
" s3://compliance-audit/audit-2025-*.zip \
  --cache-dir ~/.cache/tzip
```

**Why it saves money:** S3 Standard-IA is **$12.50/TB/month**; Glacier
Instant Retrieval is **$4/TB/month**. A ten-TB Elastic Cloud hot cluster
with the same retention runs ~$10,000-30,000/month depending on instance
sizing. Across seven years and 10 TB that's a six-figure delta per year,
without counting the SRE time.

**Why it saves time:** no rehydration workflow — the auditor's query *is*
the workflow. Phase 6's index cache keeps repeated queries sub-second after
the first cold pull.

**Why it works:** Phase 7 shard fan-out reads the Parquet columnar sidecar
inside every yearly / quarterly archive in parallel; DuckDB UNIONs them
into a single `FROM tzip` view. Phase 6's ETag-keyed cache keeps the index
entries resident so a second query doesn't re-fetch them.

---

## 2. Post-mortem observability on logs that aged out of hot storage

**Pain:** Production incident three months ago. The hot observability
stack keeps 14 days. Backup to S3 exists, but recovering it means standing
up a replica Elastic, restoring snapshots, and waiting hours. Half the
time the on-call engineer gives up and reconstructs from grep over
decompressed log files on a scratch host.

**With tzip:** the backup *is* the index. Every rotation writes a tzip
archive with term + facet + columnar indices. Three months later:

```bash
# "In the hour before the incident on 2026-08-12 14:00,
#  what endpoints saw latency spikes above the p99?"
tzip query -s "
    SELECT endpoint,
           percentile_cont(0.99) WITHIN GROUP (ORDER BY latency_ms) AS p99,
           COUNT(*) AS n
    FROM tzip
    WHERE ts BETWEEN '2026-08-12 13:00:00' AND '2026-08-12 14:00:00'
    GROUP BY endpoint
    HAVING p99 > 500
    ORDER BY p99 DESC
" s3://logs-archive/2026-08-12*.zip \
  --cache-dir ~/.cache/tzip
```

**Why it saves time:** zero restore workflow. Zero "please file a ticket
with DataEng." The query runs today, from the on-call engineer's laptop,
with the same SQL ergonomics as the hot stack. First run cold-fetches the
sidecar (one Range GET); subsequent queries hit the client cache.

**Why it saves money:** keeps the hot tier small on purpose. If the hot
tier shrinks from 90 days to 14 days because cold queries are now
first-class, that's directly ~6× infrastructure cost reduction on the
hot path — and the cold tier is 100× cheaper per GB per month.

**Why it works:** Phase 2 term index narrows candidate entries by
literal-matching keywords in the query; Phase 5 columnar sidecar lets
DuckDB do the percentile math directly; Phase 6 caches the sidecar so
the on-call's second, third, fourth query all land in RAM.

---

## 3. Hourly append-and-query for CI flake detection

**Pain:** Every CI build writes a test result JSONL. Across 200 builds a
day, 5000 tests each, that's 1M records/day. Running ClickHouse or Elastic
for "which tests are flaky this week?" is overkill; running `grep | jq` is
too slow by the time you have a quarter of history.

**With tzip Phase 5b (append-emitted segments):** one archive per week,
appended per CI run.

```bash
# At the end of each CI run:
tzip u "ci-$(date +%G-W%V).zip" "build-${CI_BUILD_ID}/results/" \
    --index-terms '*.jsonl' \
    --write-columnar                    # schema auto-loaded from base

# Weekly, from any engineer's laptop:
tzip query -s "
    SELECT test_name,
           COUNT(*) FILTER (WHERE outcome='failed') * 1.0 / COUNT(*) AS flake_rate,
           COUNT(*) AS runs
    FROM tzip
    WHERE ts >= now() - INTERVAL 7 DAY
    GROUP BY test_name
    HAVING flake_rate BETWEEN 0.01 AND 0.5
       AND runs > 10
    ORDER BY flake_rate DESC
    LIMIT 25
" ci-*.zip
```

**Why it saves money:** $0 infrastructure. The archive lives alongside
CI artifacts; the tooling is a `brew install duckdb` on the developer's
machine. No ClickHouse node, no Elastic, no ingestion pipeline.

**Why it saves time:** `tzip u --write-columnar` is a few hundred
milliseconds at the end of a multi-minute CI build — invisible overhead.
The segmented columnar (Phase 5b) means 200 appends/day produce 200
`.segN` entries without ever rebuilding the base archive. The query
UNIONs them transparently via DuckDB's `read_parquet([..])`.

**Why it works:** Phase 5b is the keystone. Without it you'd pay a full
archive rewrite per append. With it, each CI run's columnar contribution
is proportional only to that build's test count — not the week's.

---

## 4. Publishing research datasets that are queryable without infrastructure

**Pain:** A bioinformatics team runs an experiment producing 20 M JSONL
records. To share it, they either (a) dump it to S3 as loose files (every
collaborator writes their own parser), (b) stand up a shared Postgres
(provisioning cost, access control), or (c) publish a Zenodo/Figshare
record that's just a tarball (same parser problem).

**With tzip:** publish *one* `.zip`. Collaborators query it in-place:

```bash
# Publisher:
tzip create experiment-2026-Q4.zip raw-records/ \
    --index-terms '*.jsonl' \
    --facet '.tissue' --facet '.condition' \
    --write-columnar schema.json

aws s3 cp experiment-2026-Q4.zip s3://my-lab/datasets/ \
    --acl public-read

# Any collaborator:
tzip query -s "
    SELECT gene, condition,
           AVG(expression) AS mean_expr,
           STDDEV(expression) AS sd
    FROM tzip
    WHERE tissue = 'liver'
    GROUP BY gene, condition
    ORDER BY mean_expr DESC
    LIMIT 100
" s3://my-lab/datasets/experiment-2026-Q4.zip \
  --cache-dir ~/.cache/tzip
```

**Why it saves money:** $0 shared infrastructure. No database to pay for,
no auth/ACL to maintain. The dataset's S3 bucket is also its "query
server."

**Why it saves time:** a collaborator goes from "download" to "answer"
in one command. The first query cold-fetches the Parquet sidecar
(typically 1-5% of the archive size); subsequent queries hit the client
cache. No "please let me into your ES cluster" email thread.

**Why it works:** Phase 3 facets answer "what tissues are in this
dataset?" in milliseconds (`tzip aggregate --fields`). Phase 5 columnar
+ DuckDB handles the full SQL surface the researcher needs for actual
analysis. Phase 6 keeps the index local so iteration is interactive.

---

## 5. Multi-tenant SaaS: per-customer archives that *are* the data boundary

**Pain:** B2B SaaS, "please export all data for tenant 42" is a monthly
request. In a shared Postgres/Elastic the export is a long-running job
over a `WHERE tenant_id = 42` filter — slow, and a privacy footgun if
the filter ever gets dropped. GDPR deletion of tenant 42 is a similar
nightmare: row-by-row, hoping you caught every table + every index +
every backup.

**With tzip:** one archive per tenant per month. The archive IS the
data boundary.

```bash
# Rolling append through the month, from the ingest side:
tzip u "s3://tenants/42/$(date +%Y-%m).zip" "/incoming/tenant_42/" \
    --index-terms '*.jsonl' \
    --facet '.event_type' \
    --write-columnar

# "Export every event for tenant 42 in 2025":
aws s3 sync s3://tenants/42/2025-*.zip /tmp/export-tenant-42/

# "GDPR-delete tenant 42":
aws s3 rm s3://tenants/42/ --recursive

# Analytics on your own data across all tenants:
tzip query -s "
    SELECT event_type, COUNT(DISTINCT tenant_id) AS tenants_active
    FROM tzip
    WHERE ts >= '2026-11-01'
    GROUP BY event_type
    ORDER BY tenants_active DESC
" s3://tenants/*/2026-11.zip
```

**Why it saves money:** no per-tenant indexes in the shared Elastic;
no multi-GB-per-tenant JVM heap pressure. Per-tenant cost is literally
"S3 bytes for that tenant's files."

**Why it saves time:** GDPR deletion is `aws s3 rm` — a verifiable file-
level operation, not a cross-system scavenger hunt. Export is `aws s3
sync` — the customer gets a tarball of what they already paid for, no
ETL job. Legal gets a one-line audit trail.

**Why it works:** Phase 7 shard fan-out means cross-tenant analytics
(`s3://tenants/*/...`) is the same `tzip query` as a single-tenant
query, with DuckDB handling the UNION. File-level isolation is
structurally impossible to accidentally leak in SQL.

---

## 6. The hot/cold split: shrink your hot tier by 10×

**Pain:** Your Elastic / ClickHouse hot cluster holds 90 days of data
because engineers *might* need to query day 90. The actual query
frequency on days 30-90 is 1-2 per week. You're paying for always-hot
RAM + SSD to serve queries that are rare and non-urgent.

**With tzip Phase 7 as the cold path:**

- Keep 14 days hot in Elastic / ClickHouse (zero behavior change for
  dashboards).
- Every day, dump day-14 as a tzip archive to S3 and drop it from hot.

```bash
# Nightly cron on day-14 of retention:
tzip create "logs-$(date -d '14 days ago' +%F).zip" "/hot-cluster-export/$(date -d '14 days ago' +%F)/" \
    --index-terms '*.jsonl' \
    --facet '.service' --facet '.status_code' \
    --write-columnar schema.json
aws s3 cp "logs-$(date -d '14 days ago' +%F).zip" s3://logs-cold/

# Engineer's cold query (unchanged SQL, different data path):
tzip query -s "
    SELECT service, date_trunc('day', ts) AS day,
           percentile_cont(0.99) WITHIN GROUP (ORDER BY latency_ms) AS p99
    FROM tzip
    WHERE ts BETWEEN '2026-05-01' AND '2026-05-31'
    GROUP BY service, day
    ORDER BY day, p99 DESC
" s3://logs-cold/logs-2026-05-*.zip \
  --cache-dir ~/.cache/tzip
```

**Why it saves money:** shrinking a 90-day hot cluster to 14 days is a
~6× infrastructure cost reduction on the hot path. S3 cold storage is
<1% of the per-GB cost. For a shop at 5 TB/day ingestion, that's
realistically $50-200k/year in avoided cluster capacity.

**Why it saves time:** no "please restore cold data" ticket. No week-
long escalation chain to query an incident from month 2. The SQL in
the cold path is identical to the SQL in the hot path (DuckDB's
dialect is compatible with ClickHouse for 95% of OLAP patterns).

**Why it works:** Phase 5 lets cold data answer the same deep queries
as hot. Phase 6's index cache compensates for S3 RTT — the second
query after a cold fetch is sub-20ms p99 locally. Phase 7 handles
multi-day cross-shard queries without any coordinator.

---

## What these scenarios have in common

- **The archive is the database.** No extra service to run, scale, or
  secure. S3 (or any object store, or a local disk) is the backing.
- **SQL is the query language.** DuckDB handles deep aggregations,
  joins, window functions, date bucketing, percentiles — everything
  an Elastic/ClickHouse user already knows.
- **Append-safe.** Phase 5b means you never rebuild to add today's
  data. Phase 4 means the same for name/term/facet indices.
- **Shardable.** `s3://bucket/path/*.zip` is a query target; one SQL
  statement reads every matching shard in parallel.
- **Cacheable.** `--cache-dir` turns repeat queries on the same URL
  into local reads; ETag-keyed invalidation handles updated archives
  without a manual bust.
- **Portable.** The archive is one file. Ship it on a USB drive to an
  air-gapped site; download it to a laptop on an airplane; attach it
  to an audit response. It queries the same way everywhere.

## Who this isn't for

- **Sub-second ingest-to-query freshness** (sub-minute is doable with
  hourly `tzip u`; sub-second is a hot-stream problem — keep your
  Kafka/Flink/ClickHouse for that).
- **Full-text relevance ranking with BM25 + synonyms + stemming**
  (tzip grep is "does this exact term appear"; Elastic's relevance
  ranking is a different problem).
- **Transactional writes.** Appends are atomic per-update, but tzip
  is not a transactional database. Use it for append-only analytics.

The sweet spot is the 70% of data engineering problems that are
*cold-mostly, append-heavy, query-occasionally*. For those the
infrastructure cost of a real database is paying for capabilities you
don't use.
