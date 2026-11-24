# tzip scenarios

Concrete user stories where tzip changes the cost / latency math vs the "just
put the files on the filesystem / S3" default. Each is grounded in features
that already exist in tzip — the commands at the bottom of each story work
today — and in published cloud pricing / disk-level numbers.

- [S3: 500,000 training images — ZIP vs loose objects](s3-training-dataset.md)
- [NAS: 2 million small files on an inode-starved share](nas-small-files.md)
- [Logs: grep across a year of daily archives without rehydrating](log-archive-grep.md)
- [CI/CD: build-artifact distribution for 50 engineers](ci-artifact-distribution.md)
- [Cold storage: Glacier Deep Archive economics for small-file corpora](glacier-deep-archive.md)

Each story covers:
- What the pain looks like without tzip (specific syscalls / API calls /
  dollar amounts).
- The tzip feature(s) that remove it.
- Order-of-magnitude savings backed by real numbers or published pricing.
- The exact `tzip` command lines that implement the solution.

Not a sales pitch; the numbers are specific so you can sanity-check against
your own workload.
