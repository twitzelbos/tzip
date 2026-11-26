# Pulling one small entry out of a 100 GB archive on S3

## Scenario

A 100 GB tzip archive lives at `s3://ops-archives/corpus-2026-Q4.zip`.
You know the entry name — say `pages/2026-11-24/report-04812.json`,
~1 KB uncompressed — and you want to `tzip cat` it from your laptop.

The archive is big; the payload is small; the question is: **how many
bytes actually cross the wire?** The answer is "it depends — but not
on the archive size," which is the point of this doc.

---

## What crosses the wire, in order

`tzip cat s3://ops-archives/corpus-2026-Q4.zip pages/2026-11-24/report-04812.json`

1. **Length probe** — `Range: bytes=0-0` signed GET. Returns one
   body byte plus the `Content-Range: bytes 0-0/<total>` header
   (and the ETag that Phase 6 caching pins to). **~1 KB.**
2. **End of Central Directory read** — `Range: bytes=(end-64 KiB)-end`.
   The EOCD + optional ZIP64 locator + ZIP64 EOCD all land here.
   On a well-formed archive with an empty comment, the real EOCD is
   22 bytes; we take a 64 KiB window because the EOCD can sit up to
   that far back on an archive with a non-empty comment. **~1 KB on
   typical archives, up to 64 KiB worst case.**
3. **Central Directory read — full.** From `[cd_offset, cd_offset + cd_size)`,
   one signed range GET. CDR size is **~70 bytes × N_entries**,
   where N_entries is the archive's entry count — *not* its byte
   size. This is where the dominant cost lives.
4. **Local File Header read** — 30 bytes + filename + extra fields
   for the target entry. **~100 bytes.**
5. **Entry body read** — the ~1 KB JSON itself, plus the Phase-6 cache
   write-through if `--cache-dir` is set.

Total ≈ `EOCD + CDR + 2 KB`. The CDR dominates unless N_entries is tiny.

---

## The actual numbers

How much the CDR costs depends entirely on how many entries the archive
holds, not how much data it carries:

| Archive shape                            | N_entries  | CDR bytes | Transfer for one 1 KB `cat` |
|------------------------------------------|------------|-----------|-----------------------------|
| 100 × 1 GB chunks (ML datasets, DB dumps)| 100        | ~7 KB     | **~10 KB**                  |
| 100 K × ~1 MB entries (pages, documents) | 100 000    | ~7 MB     | **~7 MB**                   |
| 1 M × ~100 KB entries (fine-grained logs)| 1 000 000  | ~70 MB    | **~70 MB**                  |
| 10 M × ~10 KB entries (chat messages)    | 10 000 000 | ~700 MB   | **~700 MB**                 |

Rule of thumb: **you pay `~70 bytes × N_entries` on the wire, plus a
few KB of structural overhead, regardless of how large the archive is**.
At 100 entries the transfer is tens of KB. At 10 million entries it's
most of a gigabyte.

### Reproduce the numbers on your own bucket

`scripts/s3-full-demo.sh` ends with a measured cold-vs-cached
comparison against a 100 K-entry PPMd-max + AES-256 archive the
same script builds and uploads:

```
Appendix  CDR cache cold vs warm — the whole point of --cache-dir
1) cold tzip ls (no cache — full CDR refetch each time)
2) tzip ls --cache-dir (first call populates the CDR cache)
3) tzip ls --cache-dir (second call — CDR served from local disk)
4) cold tzip cat one entry (no cache)
5) tzip cat --cache-dir, same entry (cache hit from the ls above)
   tzip ls   speedup: X× (Y s → Z s)
   tzip cat  speedup: X× (Y s → Z s)
```

The speedups track the entry-count scale in the table above —
bigger archive → bigger CDR → bigger win. Run the script
against your own workload to see the exact numbers for your
network + bucket region.

---

## Where the structure tzip already ships *does* help

A few things in tzip offset the CDR cost — but they don't erase it:

- **`.tzip/names.v1` MPH index** (set at create with `--index-names`):
  lets `lookup_entry_idx(name)` resolve in O(1) instead of O(N_entries).
  The CDR still crosses the wire — the index just accelerates CPU
  lookup once we have it in memory. Zero bytes saved.

- **Phase 6 index cache** (`--cache-dir`): caches `.tzip/*` sidecar
  bodies, keyed by ETag so a changed archive invalidates cleanly.
  On a **repeat** `tzip cat`, the cache serves `.tzip/names.v1` from
  local disk, but the CDR itself is still refetched on every open —
  the cache is scoped to sentinel entries, not ZIP structural bytes.
  So the second cat still pays the full ~70 MB for a 1 M-entry
  archive.

- **Phase 4 segmented index / Phase 5b segmented columnar**: these
  grow the tail without rebuilding, which keeps cold-read costs
  stable across append-heavy workloads. Doesn't change the per-cat
  cost.

The real win is on `tzip query --sql`, which is a different path:
the Phase 5 columnar sidecar + PR #72's row-group prune mean a
predicate-pushed-down SQL query reads only the Parquet footer
(tens of KB) + the specific row groups whose stats match. **Your
`WHERE page_id = …` might transfer under 100 KB regardless of
archive or sidecar size.** But that's for querying, not raw
extract — different tool for different problem.

---

## When this is a problem, and what to do today

The CDR transfer cost scales with entry count. It becomes a problem
exactly when you have **many small entries in a single remote archive**.
Two practical mitigations that work today:

### 1. Shard the archive by access pattern

Instead of one 100 GB archive with 10 M entries, build N smaller
archives where each one has ≤ 100 K entries. tzip's shard fan-out
(`s3://bucket/shards/*.zip`) UNIONs them transparently at query time:

```bash
# Create one shard per day instead of one monolithic archive:
tzip create "pages-$(date +%F).zip" /data/pages/ \
    --index-names

# Fan-out ls / grep / query / aggregate across shards:
tzip ls   s3://ops-archives/pages-*.zip
tzip grep "KeyError" s3://ops-archives/pages-*.zip --index-terms '*.json'
```

Per-shard CDR stays manageable (hundreds of KB), the client only pays
the CDR of shards whose name matches the glob, and the DuckDB
`read_parquet([…])` UNION at query time means the SQL path is
transparent across shards.

### 2. Keep the client close to the archive

If you're pulling one small entry per hour, pay the ~70 MB CDR per hour.
If you're pulling thousands of entries from the same archive in a
session, open it once (via `tzip mount` on macOS 15.4+, or by keeping
a long-running `tzip ls`/`grep`/`query` process) so the parsed CDR
lives in memory and subsequent entries cost only their body bytes +
LFH (~100 bytes of overhead each).

---

## Open design: CDR caching

The structural-cost analysis above points at a real follow-up:
**ETag-keyed CDR caching** (tracked as issue #74).

- At open time, `Archive::open_source_with_cache` could consult the
  Phase 6 cache for a `CDR@<etag>` blob before issuing the CDR
  range-GET. A hit turns the second-and-later `tzip cat` on the
  1 M-entry archive from ~70 MB back down to the ~2 KB of EOCD
  probe + LFH + body.
- The ETag already covers invalidation — a changed archive
  produces a new ETag, which invalidates the cached CDR naturally.
- Cost: an extra cache bucket (two if you want to cache the EOCD
  separately, which is probably overkill — it's bounded and small).
- Scope: a modest extension of `src/index/indexcache.rs` + the
  existing `CacheCtx` plumbing on `Archive`.

### Security: does caching an ETag leak information about an encrypted archive?

Short answer: no new information disclosure beyond what the live
S3 object already exposes — but the local cache file must be
`0700`-mode so other users on the same machine can't read entry
names.

**What an ETag of a tzip AES-256 archive actually contains.**
S3 computes the ETag as `MD5(whole object bytes)` for a single
PUT, or `MD5(concat MD5 of parts) + "-N"` for multipart. The
"object bytes" are:

- ZIP structural headers (LFH, CDR, EOCD) — plaintext by ZIP
  spec.
- Entry bodies wrapped in WinZIP AE-2:
  `random 16-byte salt + pw-verify + CTR ciphertext + HMAC-SHA1[..10]`
  (see `src/zip/crypto.rs::encrypt_ae2_in_place_with_salt`).

The salt is fresh per entry per upload (`rand::thread_rng().fill_bytes`),
so identical plaintext produces different ciphertext → different
MD5 → different ETag. **The ETag doesn't leak plaintext bodies,
doesn't help a known-plaintext attack, doesn't expose the
password.**

What the ETag *does* reveal:

- The object changed at time t.
- Approximate size (correlated with but not equal to
  `Content-Length`).
- Multipart part count (the `-N` suffix).

Every one of those is already exposed by `HEAD` /
`ListObjects` to anyone with `s3:GetObject`. Keying the cache on
ETag adds zero disclosure to the network / bucket path.

**The one real concern, specific to tzip.** The ZIP spec keeps
entry names in the central directory **in plaintext even on
password-protected archives** — only entry *bodies* get
encrypted. So a cached CDR contains the entry-name list. That's
not new information on the wire (the live archive exposes it
to anyone who can read it), but it would be new information to
**other OS users on the same machine** if the cache file
inherited default `0644` permissions from the process umask.
This is already true today for the Phase 6 cache of
`.tzip/names.v1` — not a new disclosure channel — but the CDR
cache is a natural place to tighten this once for every bucket.

**Design requirements this adds to #74.**

1. Cache directory root created `0700` and cache files `0600`
   on Unix. Reuse / harden the Phase 6 cache's dir-creation
   path.
2. Cache key includes the full source URL, not just the ETag —
   two different objects with coincidentally equal fingerprints
   (improbable with MD5, more so with weak / server-specific
   fingerprints on non-S3 HTTP) must not share a slot. Phase 6
   already keys on `(archive_url, fingerprint)` via
   `ArchiveId`; reuse that.
3. Non-S3 HTTP sources that don't emit an ETag: fall back to
   `Last-Modified` → `Content-Length` → "no fingerprint
   available, skip the cache" (hard-disable, do not fall
   through to an unkeyed cache). Same chain Phase 6 uses.
4. Weak ETags (`W/"..."`) are treated as opaque strings:
   verbatim, aggressive invalidation. Worst case an extra
   cold fetch; never a correctness issue.
5. Out of scope: encrypting cache contents at rest. The data
   isn't more sensitive than `~/.cache/aws-cli/` or the OS's
   own HTTP caches; users who need encrypted local cache put
   `$HOME/.cache/tzip` on a FileVault / LUKS volume.

**Relationship to PR #70's S3 append.** Append goes through
`CompleteMultipartUpload`, which produces a new object ETag
(computed over the composed parts). The CDR cache invalidates
on that ETag change naturally — a reader sees a cache miss,
fetches the fresh CDR, repopulates. No coordination needed
between writer and reader.

---

## tl;dr for the single-entry-from-huge-archive question

The transfer bill for `tzip cat s3://100gb.zip some/entry.json` is
dominated by the central directory, which scales with
`N_entries × ~70 bytes`:

- **100 entries** → **~10 KB** (negligible)
- **100 K entries** → **~7 MB**
- **1 M entries** → **~70 MB**
- **10 M entries** → **~700 MB**

If you're routinely doing single-entry fetches from huge archives,
either **shard by access pattern** (keep per-shard entry count
under ~100 K) or **batch the fetches** in one long-lived tzip
process. CDR caching is on the roadmap and would turn repeat
single-entry fetches into "~2 KB each" but hasn't been built yet.

For SQL queries against the columnar sidecar — a different
access pattern — the costs are much smaller and already minimised
by PR #72's row-group prune.
