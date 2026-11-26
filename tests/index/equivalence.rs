//! P0 #1 — prove indexed and unindexed search equivalence.
//!
//! `tzip grep` uses `.tzip/terms.v1` (when present) to narrow the
//! candidate entries it body-scans. The index is an *optimization*:
//! for any (pattern, archive) pair, the match set the user sees
//! must be identical whether or not the index was built. If the
//! index narrowed too aggressively — a bad tokenizer, a missed
//! segment, a coverage-trailer mismatch — it would silently drop
//! lines a user had every right to expect. That'd be a correctness
//! regression masquerading as a perf feature.
//!
//! This file builds pairs of archives from randomised corpora:
//! `plain.zip` has no term index; `indexed.zip` has one built with
//! `--index-terms '*.json'`. For each of several seeds, we pick a
//! query token, grep both archives, sort the output, and assert
//! set equality.
//!
//! Randomness is seeded with a deterministic LCG so a failure is
//! trivially reproducible from the seed printed in the panic.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tzip_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tzip"))
}

fn run_tzip(cwd: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(tzip_bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run tzip");
    assert!(
        out.status.success(),
        "tzip {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

/// Minimal reproducible LCG — xorshift64* is well-mixed enough for
/// our purposes and lets the test fail-print a single `seed` value
/// the reader can paste into a bisect.
#[derive(Clone)]
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n.max(1)
    }
}

/// Tzip's term tokenizer splits on non-alphanumeric, lowercases,
/// and keeps tokens in `[3, 64]` chars. We draw from a vocabulary
/// that respects those bounds so every word we generate IS a term
/// — any divergence must be in the index path, not in our test
/// setup.
const VOCAB: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    "iota", "kappa", "lambda", "mu", "nu", "omicron", "sigma", "omega",
    "red", "green", "blue", "yellow", "purple", "orange", "pink",
    "server", "client", "request", "response", "error", "warning", "info",
    "database", "cache", "index", "query", "scan", "load", "store",
    "north", "south", "east", "west",
    "monday", "tuesday", "wednesday", "thursday", "friday",
    "unique-token-xyz",
];

fn generate_corpus(root: &Path, seed: u64, n_entries: usize) -> Vec<Vec<String>> {
    fs::create_dir_all(root.join("corpus")).unwrap();
    let mut rng = Rng::new(seed);
    let mut all_tokens: Vec<Vec<String>> = Vec::with_capacity(n_entries);
    for i in 0..n_entries {
        let words_per_entry = 3 + rng.range(8);
        let mut tokens: Vec<String> = Vec::with_capacity(words_per_entry);
        for _ in 0..words_per_entry {
            tokens.push(VOCAB[rng.range(VOCAB.len())].to_string());
        }
        // JSON-wrap so the tokenizer sees real-world punctuation
        // (quotes, braces, colons) and must still parse identical
        // tokens on both sides.
        let payload = tokens.join(" ");
        let content = format!(
            "{{\"id\":{},\"msg\":\"{}\"}}\n",
            i, payload,
        );
        fs::write(root.join(format!("corpus/e{:04}.json", i)), content).unwrap();
        all_tokens.push(tokens);
    }
    all_tokens
}

/// Run grep and return the set of `entry:line:text` triples,
/// stripping the leading `archive:` prefix so plain.zip and
/// indexed.zip compare apples-to-apples.
fn grep_lines(cwd: &Path, archive: &str, token: &str) -> BTreeSet<String> {
    let out = run_tzip(cwd, &["grep", token, archive]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.strip_prefix(&format!("{}:", archive))
                .unwrap_or(l)
                .to_string()
        })
        .collect()
}

fn run_equivalence_pair(seed: u64, n_entries: usize, pattern: &str) {
    let tmp = tempfile::tempdir().unwrap();
    generate_corpus(tmp.path(), seed, n_entries);

    run_tzip(
        tmp.path(),
        &["create", "plain.zip", "corpus", "-m", "deflate", "-q"],
    );
    run_tzip(
        tmp.path(),
        &[
            "create",
            "indexed.zip",
            "corpus",
            "-m",
            "deflate",
            "--index-terms",
            "*.json",
            "-q",
        ],
    );

    let plain = grep_lines(tmp.path(), "plain.zip", pattern);
    let indexed = grep_lines(tmp.path(), "indexed.zip", pattern);
    assert_eq!(
        plain, indexed,
        "indexed grep ≠ unindexed grep on pattern {:?}, seed = {}",
        pattern, seed,
    );
}

#[test]
fn equivalence_random_seeds_present_tokens() {
    // Pick tokens that are drawn from the vocabulary → almost
    // always present somewhere. The index should narrow but never
    // drop a real match.
    for seed in 1u64..=10 {
        let mut rng = Rng::new(seed.wrapping_mul(0xdeadbeef));
        let token = VOCAB[rng.range(VOCAB.len())];
        run_equivalence_pair(seed, 40, token);
    }
}

#[test]
fn equivalence_absent_token_empty_on_both_sides() {
    // A token that is NOT in the vocabulary: the plain grep must
    // return 0 matches, and the indexed grep must ALSO return 0 —
    // but via the index narrowing the candidate set to empty
    // rather than through scanning every body.
    for seed in 100u64..=105 {
        run_equivalence_pair(seed, 40, "absolutely-absent-marker-qqqq");
    }
}

#[test]
fn equivalence_rare_token_appears_once() {
    // Plant exactly one occurrence of a unique marker into one
    // random entry; ensure both the indexed and unindexed grep
    // find exactly it.
    for seed in 200u64..=204 {
        let tmp = tempfile::tempdir().unwrap();
        generate_corpus(tmp.path(), seed, 60);
        // Overwrite one entry to contain the marker.
        let mut rng = Rng::new(seed.wrapping_mul(0xa3));
        let idx = rng.range(60);
        let marker = "unique-marker-zzz-0007";
        fs::write(
            tmp.path()
                .join(format!("corpus/e{:04}.json", idx)),
            format!("{{\"id\":{},\"msg\":\"{}\"}}\n", idx, marker),
        )
        .unwrap();

        run_tzip(
            tmp.path(),
            &["create", "plain.zip", "corpus", "-m", "deflate", "-q"],
        );
        run_tzip(
            tmp.path(),
            &[
                "create",
                "indexed.zip",
                "corpus",
                "-m",
                "deflate",
                "--index-terms",
                "*.json",
                "-q",
            ],
        );

        let plain = grep_lines(tmp.path(), "plain.zip", marker);
        let indexed = grep_lines(tmp.path(), "indexed.zip", marker);
        assert_eq!(plain, indexed, "seed {} rare-token divergence", seed);
        assert_eq!(plain.len(), 1, "exactly one match expected, seed {}", seed);
    }
}

#[test]
fn equivalence_mixed_coverage_log_and_json() {
    // If `--index-terms '*.json'` only covers json entries, a .log
    // entry's content must STILL be scanned by the grep. Dropping
    // non-covered entries from the plain-scan path would be the
    // most dangerous regression — a user grepping an archive would
    // silently miss matches in files the index chose to ignore.
    for seed in 300u64..=304 {
        let tmp = tempfile::tempdir().unwrap();
        let n_json = 20;
        generate_corpus(tmp.path(), seed, n_json);

        // Also plant a .log entry with a common vocab word.
        let marker_word = "lambda";
        fs::write(
            tmp.path().join("corpus/trailer.log"),
            format!("INFO {} happens here\n", marker_word),
        )
        .unwrap();

        run_tzip(
            tmp.path(),
            &["create", "plain.zip", "corpus", "-m", "deflate", "-q"],
        );
        run_tzip(
            tmp.path(),
            &[
                "create",
                "indexed.zip",
                "corpus",
                "-m",
                "deflate",
                "--index-terms",
                "*.json",
                "-q",
            ],
        );

        let plain = grep_lines(tmp.path(), "plain.zip", marker_word);
        let indexed = grep_lines(tmp.path(), "indexed.zip", marker_word);
        assert_eq!(
            plain, indexed,
            "seed {}: `--index-terms '*.json'` must not hide matches in .log files",
            seed,
        );
        assert!(
            indexed.iter().any(|l| l.contains("trailer.log")),
            "seed {}: expected the .log entry to be scanned despite being outside the index's coverage glob; got {:?}",
            seed,
            indexed,
        );
    }
}

#[test]
fn equivalence_case_insensitive_matches_agree() {
    // `tzip grep -i` + the term-index tokenizer (which lowercases
    // input) are the two independent paths that must agree.
    for seed in 400u64..=403 {
        let tmp = tempfile::tempdir().unwrap();
        generate_corpus(tmp.path(), seed, 30);

        // Add an entry with an UPPERCASE occurrence of a vocab word.
        fs::write(
            tmp.path().join("corpus/UPPER.json"),
            "{\"id\":999,\"msg\":\"SERVER crashed hard\"}\n",
        )
        .unwrap();

        run_tzip(
            tmp.path(),
            &["create", "plain.zip", "corpus", "-m", "deflate", "-q"],
        );
        run_tzip(
            tmp.path(),
            &[
                "create",
                "indexed.zip",
                "corpus",
                "-m",
                "deflate",
                "--index-terms",
                "*.json",
                "-q",
            ],
        );

        let plain = run_tzip(
            tmp.path(),
            &["grep", "-i", "server", "plain.zip"],
        );
        let indexed = run_tzip(
            tmp.path(),
            &["grep", "-i", "server", "indexed.zip"],
        );
        let p: BTreeSet<String> = String::from_utf8_lossy(&plain.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.strip_prefix("plain.zip:").unwrap_or(l).to_string())
            .collect();
        let i: BTreeSet<String> = String::from_utf8_lossy(&indexed.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.strip_prefix("indexed.zip:").unwrap_or(l).to_string())
            .collect();
        assert_eq!(
            p, i,
            "seed {}: -i case-insensitive grep must agree indexed vs unindexed",
            seed,
        );
        assert!(
            p.iter().any(|l| l.contains("UPPER.json")),
            "seed {}: expected UPPERCASE hit, got {:?}",
            seed,
            p,
        );
    }
}
