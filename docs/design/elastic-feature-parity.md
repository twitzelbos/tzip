# Design: closing the gap to Elasticsearch

## Goal

Make every row in the Elastic-vs-tzip comparison table a ✓ for tzip —
realistically, and without turning tzip into a cluster.

| Workload | Elastic | tzip today | Status |
|---|---|---|---|
| Cold-corpus sub-second search | ✓ | ✓ | **shipped** — Phase 1 (#52) + Phase 2 (#53) |
| Basic facets + top-N + histograms | ✓ | ✓ | **shipped** — Phase 3 (#55) |
| Append + visible in seconds | ✓ | ✓ | **shipped** — Phase 4 (#56) |
| p99 < 20 ms on hot 10B-doc index | ✓ | ✓ (cached) | **shipped** — Phase 6 (#58) |
| Deep nested aggregations | ✓ | ✓ | **shipped** — Phase 5 (#57, DuckDB) |
| Horizontal shard parallelism | ✓ | ✓ | **shipped** — Phase 7 (#59) |
| No cluster to run, archive travels | ✗ | ✓ | always |
| Full-corpus storage cost | ~3× raw | 1× compressed | always |

**All seven phases landed.** See the per-phase sections below for what each
PR delivers; the roadmap is maintained as the design doc of record.

The design below assumes tzip's write-once-and-append archive model —
not real-time stream ingestion at Elastic's rate. For the archive-at-
rest + cold / warm search use case, the plan below delivers Elastic-
class query ergonomics with no cluster to operate.

## Non-goals

- Live dashboard refresh at 10 Hz against a write-heavy stream.
- Full BM25 relevance ranking with field boosting / stemming / synonyms.
  (tzip's model is "does this term appear" with optional term-frequency
  scoring, not "which document is MOST relevant.")
- Cross-archive transactions.
- Replace Elastic for every workload — rather, replace it for the 70% of
  workloads that are append-mostly + cold-query-mostly, and
  complement it for the rest.

## Shape of the index

All indices live **inside** the tzip archive as special reserved entries
named `.tzip/<index-kind>.v<N>`. They are first-class ZIP entries so
every tool can list / copy / sign them, but tzip handles them specially
at read time:

- `.tzip/names.v1` — MPH over CDR entry names → entry index. O(1) lookup
  by name.
- `.tzip/terms.v1` — hashed inverted index: `hash(term) → posting list`
  for every text entry. Drives `tzip grep EXACT_TERM archive.zip`.
- `.tzip/facets.v1/<field>` — per-field term dictionary + count + min /
  max / cardinality metadata. Drives `tzip aggregate`.
- `.tzip/schema.v1` — declares which fields were indexed, which types
  (string / number / date), which analyzers were applied.
- `.tzip/columnar.v1.parquet` — optional Parquet sidecar of extracted
  structured fields. Drives deep aggregations via embedded DuckDB.

Index entries are **optional**. Archives without them still work
identically to today; readers fall back to the linear-scan paths.

## Phases

Each phase is one PR, shippable independently, each adding one row to
the ✓ column.

### Phase 1: MPH on entry names → O(1) name lookup — SHIPPED in #52

**Deliverable:** `tzip create --index-names` writes a Minimal Perfect
Hash over entry names into `.tzip/names.v1`. `Archive::open` reads it
on open; `tzip cat NAME archive.zip` / `tzip x` / entry-lookup paths
become O(1) instead of O(N).

**Cost:** ~1-2 bits per entry in the archive. 1M entries ≈ 250 KB
index. Build time: ~0.3 seconds per million names.

**Crate:** `boomphf = "0.6"` (pure Rust, ships well).

**This is the smallest shippable piece of the roadmap and lands the
first row of ✓** — the "no cluster, archive travels, fast lookup"
story — without touching any other subsystem.

### Phase 2: Hashed inverted term index → sub-second `tzip grep` — SHIPPED in #53

**Deliverable:** `tzip create --index-terms '*.json' --index-terms '*.log'`
tokenizes matching text entries, builds `hash(term) → [(entry_id,
entry_local_offset)]` postings in `.tzip/terms.v1`. `tzip grep` with
an exact-literal query (no regex, no case folding by default)
consults the index first, Range-GETs only the entries that have the
hash, scans within those for exact match.

**Fallback:** regex queries, prefix queries, substring queries that
aren't literal-term matches still linear-scan — the index is a
filter, not a ranker.

**Cost:** index typically 5-15% of corpus size (depends on token
cardinality). Build time: single-digit MB/s per core, parallel in a
rayon pool.

**Pre-existing infrastructure:** `tzip grep` already has outer-glob
fan-out, per-worker body-stream, `--checkpoint/--resume`. The index
just precedes the body-stream stage.

### Phase 3: Facets + top-N + histograms — SHIPPED in #55

**Deliverable:** `tzip create --facet '.user_id' --facet '.status_code'`
builds per-field term dictionaries + counts. `tzip aggregate --by
status_code --count archive.zip` returns `{200: 1_443_921, 500: 912,
...}` as JSON. Supports:

- `--count` (histogram of distinct values)
- `--top-n N`
- `--min / --max` on numeric fields
- `--date-histogram --interval=1h` on date fields

**Cost:** ~1% of corpus per faceted field on a typical structured
log. Query cost: O(distinct-values), not O(records).

**This picks up the "basic facets + top-N + histograms" row in the
table.**

### Phase 4: Appendable index segments — SHIPPED in #56

**Deliverable:** `tzip u archive.zip new.json` appends new entries AND
emits a new index segment file (`.tzip/names.v1.seg2`,
`.tzip/terms.v1.seg2`, etc.). Readers walk every segment when
answering a query. `tzip u --compact` merges segments into one in
the background.

**Cost:** segment init is small (minute buckets' worth of docs
produces a small segment). Linear query cost in number of segments,
but typically ≤ 10 segments steady-state with an hourly compact.

**This picks up the "append + visible in seconds" row** — the index
stays coherent across incremental updates, no rebuild needed.

### Phase 5: DuckDB sidecar for deep aggregations — SHIPPED in #57

**Known limitation:** `tzip u` doesn't refresh the columnar sidecar.
Rebuild-mode byte-copies kept entries without decompressing, so
re-extracting their JSON for the Parquet rebuild would need a
parallel decompression path. Workaround: delete the stale
`.tzip/columnar.v1.parquet` from the archive (via `tzip u --delete
'.tzip/columnar.v1.parquet'`) and re-create with
`--write-columnar schema.json`. A future segmented columnar
(analogous to Phase 4 `.segN`) would avoid this.

**Deliverable:** `tzip create --write-columnar 'schema.json'` (or
auto-infer schema from first N JSON entries) extracts declared
fields into a Parquet file stored as `.tzip/columnar.v1.parquet`.
`tzip query --sql "SELECT status, COUNT(*) FROM tzip WHERE ts >
'2026-01-01' GROUP BY status" archive.zip` dispatches to an embedded
DuckDB reading the sidecar.

**Cost:** Parquet sidecar ~20-40% of raw structured-data size
(depends on cardinality and column types). DuckDB adds ~1 MB to
binary size; alternative: external `duckdb` tool invoked only when
`--sql` is used.

**This closes the "deep nested aggregations" gap** — DuckDB's SQL
covers everything Elastic's aggregation DSL does, often faster.

### Phase 6: Hot-index caching + persistent connections → sub-20ms p99 — SHIPPED in #58

**Deliverable:** `tzip query --cache-dir ~/.cache/tzip
s3://.../archive.zip` keeps the index entries resident on the client
in an LRU cache. First query cold-fetches the index; subsequent
queries hit RAM. Add HTTP/2 persistent connection pool to S3 so
cold-fetches don't pay TLS handshake per request.

**Target:** p99 < 20 ms on cached-index + bodied lookups. S3 RTT
floor (~15 ms min for Range GET) is the bottleneck on cold-body
paths; keeping bodies in the cache too covers that.

**This picks up the "p99 < 20 ms" row for hot queries** — not against
a cold S3 archive, but against archives the client has queried
recently.

### Phase 7: Horizontal shard parallelism via archive-per-shard — SHIPPED in #59

**Deliverable:** `tzip query --shards 's3://bucket/shards/*.zip'`
runs the query across every matching archive in parallel, merges
results. Natural fit because `tzip grep` already supports glob
fan-out — just plumb index-aware queries through the same path.

**Target:** `tzip query` scales linearly with shard count up to
network / S3 throughput limit.

**This picks up the "horizontal shard parallelism" row** — not via a
distributed query coordinator, but via embarrassingly-parallel
archive-per-shard with client-side merge.

## Rollout + versioning

- Index format versioned via the filename (`.v1`, `.v2`, ...). Readers
  that don't understand a version warn and fall back to linear scan.
- `tzip --version` reports index-schema capabilities.
- `tzip archive-info` surfaces index presence / version / size.
- No breaking changes to the ZIP wire format — existing tools still
  see the index entries as plain-named ZIP entries that happen to have
  names starting with `.tzip/`.

## Honest trade-offs

tzip's index model fundamentally differs from Elastic's:

- **Index at rest**, not in RAM. Hot queries need the cache (Phase 6) to
  match Elastic's p99.
- **Static index per segment**, not a live mutable tree. Incremental
  updates work (Phase 4) but segment counts grow; `--compact` is the
  equivalent of Lucene merges, and running `--compact` nightly covers
  the gap.
- **No distributed coordinator**. Shard fan-out is client-side (Phase 7);
  for 100+ shard queries Elastic's cluster-local pre-aggregation is
  still faster, but for the common 1-20 shard case tzip's model wins on
  simplicity and cost.

## When this makes the table true

Phases 1–7 land ✓ for every row in the table (per-row ✓ noted above).
After Phase 7 we have:

- Sub-second search over cold archives via the inverted index
- Facets + aggregations via DuckDB sidecar
- Seconds-behind "live" via incremental segments
- Sub-20ms p99 on hot cached queries
- Linear horizontal scale via archive-per-shard
- Zero infrastructure — the archive is the database

That is "Elastic for 70% of workloads, without the cluster."

## Delivery

All seven phases shipped between PR #52 (Phase 1) and PR #59 (Phase 7),
each phase an independently reviewable PR with its own test suite. The
Phase-6 HTTP/2 goal was adjusted to persistent-connection pooling (which
is what the roadmap deliverable actually wanted — "no TLS handshake per
Range GET") rather than a wire-protocol upgrade; ureq 3.x is still
HTTP/1.1 and a reqwest/hyper migration would have forced a tokio bridge
disproportionate to the gain.

As of the Phase-7 merge the Elastic-vs-tzip comparison table is all ✓
for tzip: sub-second search, facets, segmented append, DuckDB-backed
deep aggregations, hot p99 under 20 ms with client-side cache, and
horizontal shard fan-out — with no cluster to operate.
