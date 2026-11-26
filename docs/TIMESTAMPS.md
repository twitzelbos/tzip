# Timestamps in a tzip archive

Short reference for what `tzip` stores per entry, what it doesn't,
and what the two different "latest" questions actually answer.
Covers issue #77 ("date time added and date time modified") and
is the semantic foundation `tzip tail` (issue #71) rests on.

---

## What each entry carries

Every entry in a tzip archive has **one** timestamp, stored in
two places that must agree:

| Where              | Format                | Resolution          |
|--------------------|-----------------------|---------------------|
| Local File Header  | DOS date + DOS time   | 2 seconds           |
| Central Directory  | DOS date + DOS time   | 2 seconds           |

That timestamp is **the source file's mtime at the moment the
archive was written**. It is not the create-time of the source
file, it is not the archive's write-time, and it does not change
if the entry's content changes in a later `tzip u`.

### Optional nanosecond-precision mtime

When the host OS supports it, tzip also emits an **Info-ZIP Unix
extra field** (UT) carrying the mtime as a 32-bit Unix timestamp.
Readers that know about the UT field use it in preference to the
DOS fields; readers that don't see a plain DOS timestamp.

Neither encoding carries the atime or ctime. The UT spec allows
them in principle; tzip doesn't emit them (they rarely survive
archive round-trips and most archiving tools ignore them).

---

## What ZIP does *not* record

- **Archive-write time** ("when was this entry added to this
  archive?") — no field. The ZIP spec has nowhere to put it.
- **Previous entry mtime** ("was this entry updated by a later
  `tzip u`?") — no history. Each entry carries exactly one
  timestamp, the one that was current when its most recent
  write happened.
- **Create time of the source file** — not represented. Even on
  filesystems that expose birthtime, tzip doesn't record it
  because the field doesn't exist in ZIP's format.
- **Archive-level timestamps** — no "archive created at",
  "archive last modified at". If you need this, use the
  filesystem's own stat() on the archive file, or the S3
  object's `Last-Modified` header.

If you need an archive-write timestamp per entry (so a later
reader can tell which `tzip u` call brought each entry in), that
would require a tzip-native extra field — a design the repo
hasn't needed yet and treats as a potential future addition.
The current workaround is "CDR position is a proxy for
archive-write order on append-only histories" — see below.

---

## Two "latest" questions, two answers

### "Latest **modified**" — use `--by mtime`

Sort entries by the DOS mtime stored on them. This gives you the
archive's view of the *source files'* most recent modifications,
in the order the filesystem reported them when each entry was
written:

```bash
tzip tail --by mtime -n 10 archive.zip
```

This is well-defined and portable: every ZIP reader everywhere
can tell you the entry mtime, and it's what the `Modified`
column in `tzip ls` already shows.

### "Latest **added**" — use `--by added` (default)

ZIP has no "added at" field, so `tzip tail --by added` uses the
entry's **position in the central directory** as a proxy:

```bash
tzip tail -n 10 archive.zip
```

This is a proxy, not a measurement. It is **exact** when the
archive's history is append-only — tzip's `tzip u` append mode
keeps all old entries at their original CDR positions and lays
new entries after them, so "last in CDR order" ≈ "most recently
added."

It is **wrong** after any of these:

- `tzip u --delete …` (triggers rebuild mode; CDR is rewritten).
- `tzip u --sort` (explicitly reorders).
- `tzip u --atomic` (also rebuild mode).
- `tzip u <new.txt>` where `new.txt`'s name collides with an
  existing entry (triggers rebuild mode to replace in place).
- Anything that regenerates the archive from scratch — rebuilds
  reorder the CDR in their own output order, not in the
  originals' added-at order.

`tzip tail --by added` on a rebuilt archive still returns the
last N CDR entries; those entries just weren't necessarily the
N most recently added to the original history.

### What about `.tzip/*` sentinels?

`tzip tail` hides them. The sentinel entries (`.tzip/names.v1`,
`.tzip/terms.v1`, `.tzip/facets.v1/*`,
`.tzip/columnar.v1.parquet`, and their `.segN` variants) are
tzip's own bookkeeping; counting one as "most recently added"
would be noise. `tzip find` and `tzip ls` surface them, because
those commands are literal CDR tours.

---

## Programs that want structured output

`tzip tail --json` emits one object per line with the fields
`archive`, `entry`, `size`, `mtime` (Unix seconds), `encrypted`,
`method`. Example:

```json
{"archive":"logs.zip","entry":"2026-11-25.jsonl","size":12345,"mtime":1764086400,"encrypted":false,"method":8}
```

That's also the shape `tzip find --json` produces, so the two
commands compose: use `find` for "every matching entry" and
`tail` for "the newest N of them."

---

## Design constraint summary

- ZIP spec forces one timestamp per entry. We can't fix that in
  the archive format without breaking compatibility.
- We can add an archive-write-time extra field in a
  tzip-native namespace; readers that don't know about it see
  a normal entry. That's a possible follow-up when someone has
  a concrete use case that the CDR-position proxy doesn't serve.
- "Latest mtime" is a proper measurement; "latest added" is a
  proxy. Both are useful, both are available, and `tzip tail`
  surfaces each with its own flag so you know which question
  you're asking.
