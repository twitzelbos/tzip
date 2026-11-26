//! S3 archive source — `Read + Seek` backed by signed Range GETs.
//!
//! Composition: URL parsing + credential discovery + per-request SigV4 +
//! ureq transport + the same 256 KiB prefetch cache as `HttpSource`.
//!
//! Credential chain:
//!   1. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` env vars (+ optional
//!      `AWS_SESSION_TOKEN`).
//!   2. `AWS_PROFILE` → `~/.aws/credentials` INI section
//!      (`aws_access_key_id` / `aws_secret_access_key` /
//!      `aws_session_token`).
//! Region: `AWS_REGION` → `AWS_DEFAULT_REGION` → `~/.aws/config` for the
//! chosen profile → fallback `us-east-1`.
//! Endpoint override: `AWS_ENDPOINT_URL` for MinIO / LocalStack.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::{SystemTime, UNIX_EPOCH};

use super::sigv4::{encode_key, sign_get, ExtraHeader, SigV4Credentials};

/// Floor on how many bytes to pull per S3 Range GET. S3 bills
/// per *request*, not per byte, so one larger fetch is strictly
/// cheaper than several small ones of the same region. The
/// window is adaptive (see `fetch`): sequential access doubles
/// it up to `PREFETCH_MAX`; a non-sequential jump resets it
/// back to `PREFETCH_MIN`. Floor is sized to keep single-entry
/// `cat` cheap without paying for round-trips on every 8 KiB
/// Read call.
const PREFETCH_MIN: u64 = 256 * 1024;

/// Cap on the adaptive prefetch window. 16 MiB turns a
/// forward-sweep over a ~100 MiB central directory from roughly
/// 100 Range GETs into roughly 6. Big enough to amortise S3's
/// per-request overhead across a sequential run, small enough
/// that a single mis-predicted sequential prefix on a random
/// workload wastes bounded bytes.
const PREFETCH_MAX: u64 = 16 * 1024 * 1024;

pub struct S3Source {
    agent: ureq::Agent,
    scheme: String,
    /// Host header value — bare "bucket.s3.region.amazonaws.com" for the
    /// virtual-hosted-style default, or "127.0.0.1:9000" (no bucket) for
    /// path-style endpoints via `AWS_ENDPOINT_URL`.
    host: String,
    /// URI path including a leading `/`. For virtual-hosted style this is
    /// `/<encoded-key>`. For path-style (`AWS_ENDPOINT_URL` set) it is
    /// `/<bucket>/<encoded-key>` — the bucket name is embedded here so the
    /// signature and URL agree.
    uri_path: String,
    creds: Credentials,
    region: String,
    len: u64,
    pos: u64,
    cache_start: u64,
    cache: Vec<u8>,
    /// ETag of the S3 object captured at open time. S3 always emits
    /// one; absent only if the upstream (MinIO / LocalStack) stripped
    /// it. Used as the index cache's content-change fingerprint
    /// (Phase 6) **and** as the `If-Match` header on every subsequent
    /// Range GET so a mid-run replacement of the object surfaces as
    /// a HTTP 412 instead of silently mixing bytes from two
    /// versions.
    fingerprint: Option<String>,
    /// Adaptive prefetch window. Grows (×2) on sequential access
    /// up to `PREFETCH_MAX`; resets to `PREFETCH_MIN` on a
    /// non-sequential jump. The intent: CDR scans and sorted-
    /// candidate greps pay a handful of large GETs; single-entry
    /// `cat`s stay cheap at the floor.
    prefetch_window: u64,
    /// End offset of the previous fetch (byte just past the
    /// cached window). `None` before the first fetch. Used only
    /// as the "is this read sequential?" predicate.
    last_fetch_end: Option<u64>,
}

/// Returned by `fetch` when the server rejected a Range GET with
/// HTTP 412 Precondition Failed — the archive we opened has been
/// replaced since open time. Surfaced to callers so grep /
/// aggregate / query / jq can bail with a clear message instead
/// of retrying or silently serving bytes from two versions of the
/// object.
pub const SNAPSHOT_CHANGED_MARKER: &str = "tzip: remote snapshot changed mid-run";

pub(crate) struct Credentials {
    pub(crate) access_key: String,
    pub(crate) secret_key: String,
    pub(crate) session_token: Option<String>,
}

impl S3Source {
    /// `spec` is whatever came after `s3://` (e.g. `my-bucket/path/key.zip`).
    pub fn open(spec: &str) -> Result<Self> {
        let (bucket, key) = split_bucket_key(spec)?;
        let creds = resolve_credentials().context("resolve AWS credentials")?;
        let region = resolve_region();

        // Endpoint + host derivation. For path-style (AWS_ENDPOINT_URL),
        // the bucket is embedded in the URI path, not in the host.
        let (scheme, host, path_style_prefix) = resolve_endpoint(&bucket, &region)?;
        let uri_path = if let Some(prefix) = path_style_prefix {
            format!("{}/{}", prefix, encode_key(&key))
        } else {
            format!("/{}", encode_key(&key))
        };

        let agent = super::shared_agent();

        // Probe object length via a Range: bytes=0-0 request so we work
        // identically against real S3 + MinIO + LocalStack (HEAD against
        // some of those returns odd things).
        let mut src = Self {
            agent,
            scheme,
            host,
            uri_path,
            creds,
            region,
            len: 0,
            pos: 0,
            cache_start: 0,
            cache: Vec::new(),
            fingerprint: None,
            prefetch_window: PREFETCH_MIN,
            last_fetch_end: None,
        };
        src.probe_len()?;
        Ok(src)
    }

    /// URL of the opened object (scheme + host + uri path).
    /// Used by Phase 6 cache code to derive the archive ID.
    pub fn url(&self) -> String {
        self.object_url()
    }

    /// ETag captured at open time; `None` on backends that didn't
    /// emit one.
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    /// Construct an `S3Source` with a caller-supplied object length,
    /// skipping the Range-0-0 probe. Used by extract / grep inner
    /// workers: the main thread has already opened the archive, so
    /// the length is known and every worker's own probe would be a
    /// wasted signed-GET round-trip. Credential + endpoint resolution
    /// still runs per worker (cheap, and credentials can rotate).
    pub fn open_with_len(spec: &str, len: u64) -> Result<Self> {
        Self::open_with_len_and_if_match(spec, len, None)
    }

    /// Same as `open_with_len`, but also pins the worker to the
    /// ETag the main-thread open captured. Every Range GET sends
    /// `If-Match: <etag>`; a mid-run replace of the object
    /// surfaces as HTTP 412 on the first fetch, so a grep /
    /// extract run reading from multiple workers can't silently
    /// mix bytes from two versions.
    pub fn open_with_len_and_if_match(
        spec: &str,
        len: u64,
        if_match: Option<String>,
    ) -> Result<Self> {
        let (bucket, key) = split_bucket_key(spec)?;
        let creds = resolve_credentials().context("resolve AWS credentials")?;
        let region = resolve_region();
        let (scheme, host, path_style_prefix) = resolve_endpoint(&bucket, &region)?;
        let uri_path = if let Some(prefix) = path_style_prefix {
            format!("{}/{}", prefix, encode_key(&key))
        } else {
            format!("/{}", encode_key(&key))
        };
        let agent = super::shared_agent();
        Ok(Self {
            agent,
            scheme,
            host,
            uri_path,
            creds,
            region,
            len,
            pos: 0,
            cache_start: 0,
            cache: Vec::new(),
            fingerprint: if_match,
            prefetch_window: PREFETCH_MIN,
            last_fetch_end: None,
        })
    }

    fn probe_len(&mut self) -> Result<()> {
        let signed = self.sign(&[ExtraHeader { name: "Range", value: "bytes=0-0" }])?;
        let mut req = self.agent.get(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let resp = req
            .call()
            .map_err(|e| anyhow!("S3 probe failed: {}", e))
            .with_context(|| format!("probe {}", self.object_url()))?;
        let status = resp.status();
        if status != 206 && status != 200 {
            bail!("S3 probe returned unexpected status {}", status);
        }
        let cr = resp
            .header("Content-Range")
            .ok_or_else(|| anyhow!("S3 probe response missing Content-Range"))?
            .to_string();
        // Capture the S3 ETag for the index cache (Phase 6) before
        // consuming the response body.
        self.fingerprint = resp.header("ETag").map(|s| s.to_string());
        let total = cr
            .split('/')
            .nth(1)
            .and_then(|t| t.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("could not parse S3 Content-Range {:?}", cr))?;
        self.len = total;
        // Keep the single byte we pulled as initial cache.
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf)?;
        self.cache_start = 0;
        self.cache = buf;
        Ok(())
    }

    fn object_url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.uri_path)
    }

    fn sign(&self, extra: &[ExtraHeader]) -> Result<super::sigv4::SignedRequest> {
        let creds = SigV4Credentials {
            access_key: &self.creds.access_key,
            secret_key: &self.creds.secret_key,
            session_token: self.creds.session_token.as_deref(),
            region: &self.region,
            service: "s3",
        };
        let now = utc_now();
        sign_get(&creds, &self.scheme, &self.host, &self.uri_path, "", extra, now)
    }

    fn fetch(&mut self, start: u64, min_len: u64) -> io::Result<()> {
        if start >= self.len {
            self.cache_start = start;
            self.cache.clear();
            return Ok(());
        }
        // Adaptive prefetch: if this fetch continues exactly
        // where the last one ended, we're in a sequential sweep
        // (CDR scan, sorted-candidate grep, parallel-block
        // decompress read-ahead). Double the window up to
        // PREFETCH_MAX. On a non-sequential jump the window
        // resets to PREFETCH_MIN so random single-entry access
        // stays cheap.
        if self.last_fetch_end == Some(start) {
            self.prefetch_window = (self.prefetch_window * 2).min(PREFETCH_MAX);
        } else {
            self.prefetch_window = PREFETCH_MIN;
        }
        let want = min_len.max(self.prefetch_window);
        let end = (start + want).min(self.len);
        self.last_fetch_end = Some(end);
        let range = format!("bytes={}-{}", start, end - 1);
        // Transient-failure retry loop. DNS resolver glitches,
        // connection resets, and 5xx responses are all recovered
        // on a second attempt the vast majority of the time;
        // ureq's default 60s timeout per attempt means one
        // flaky lookup can otherwise stall a grep worker for a
        // minute. 3 attempts with short backoff.
        let mut last_err: Option<io::Error> = None;
        for attempt in 0u32..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    200u64 * (1 << attempt),
                ));
            }
            // Re-sign each attempt — the x-amz-date header must
            // be within SigV4's 15-minute clock skew window,
            // and a long retry chain could outrun it.
            //
            // If we captured an ETag at open time, send it as
            // `If-Match` on every GET. A mid-run object
            // replacement returns 412 and we bail with a
            // snapshot-changed error — far better than silently
            // stitching two versions together.
            let mut extras: Vec<ExtraHeader> =
                vec![ExtraHeader { name: "Range", value: &range }];
            if let Some(ref etag) = self.fingerprint {
                extras.push(ExtraHeader { name: "If-Match", value: etag });
            }
            let signed = match self.sign(&extras) {
                Ok(s) => s,
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            };
            let mut req = self.agent.get(&signed.url);
            for (k, v) in &signed.headers {
                req = req.set(k, v);
            }
            match req.call() {
                Ok(resp) => {
                    if resp.status() != 206 && resp.status() != 200 {
                        if resp.status() >= 500 && attempt < 2 {
                            last_err = Some(io::Error::new(
                                io::ErrorKind::Other,
                                format!("S3 Range GET returned status {}", resp.status()),
                            ));
                            continue;
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("S3 Range GET returned status {}", resp.status()),
                        ));
                    }
                    let mut buf = Vec::with_capacity((end - start) as usize);
                    resp.into_reader().read_to_end(&mut buf)?;
                    if buf.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("empty S3 body for Range {}", range),
                        ));
                    }
                    self.cache_start = start;
                    self.cache = buf;
                    return Ok(());
                }
                Err(ureq::Error::Status(412, _)) => {
                    // Precondition failed → the object we
                    // opened has been replaced. Not retryable;
                    // surface a clear error up through Read.
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "{} (ETag {} no longer matches on {})",
                            SNAPSHOT_CHANGED_MARKER,
                            self.fingerprint.as_deref().unwrap_or("?"),
                            self.object_url(),
                        ),
                    ));
                }
                Err(e) => {
                    last_err = Some(io::Error::new(io::ErrorKind::Other, e));
                    // Fall through to retry.
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "S3 Range GET failed after retries")
        }))
    }
}

impl Read for S3Source {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        if self.pos >= self.cache_start {
            let rel = (self.pos - self.cache_start) as usize;
            if rel < self.cache.len() {
                let avail = &self.cache[rel..];
                let n = avail.len().min(out.len());
                out[..n].copy_from_slice(&avail[..n]);
                self.pos += n as u64;
                return Ok(n);
            }
        }
        self.fetch(self.pos, out.len() as u64)?;
        if self.cache.is_empty() {
            return Ok(0);
        }
        let rel = (self.pos - self.cache_start) as usize;
        let avail = &self.cache[rel..];
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for S3Source {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos: i64 = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative offset",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

// ── URL / credential / region / endpoint helpers ───────────────────────────

fn split_bucket_key(spec: &str) -> Result<(String, String)> {
    // spec is the bit after "s3://". Expect "bucket/key..." — no leading /.
    let (bucket, key) = spec
        .split_once('/')
        .ok_or_else(|| anyhow!("s3 URL must be s3://bucket/key — got s3://{}", spec))?;
    if bucket.is_empty() {
        bail!("empty bucket in s3://{}", spec);
    }
    if key.is_empty() {
        bail!("empty key in s3://{}", spec);
    }
    Ok((bucket.to_string(), key.to_string()))
}

pub(crate) fn resolve_credentials() -> Result<Credentials> {
    if let (Ok(ak), Ok(sk)) = (
        std::env::var("AWS_ACCESS_KEY_ID"),
        std::env::var("AWS_SECRET_ACCESS_KEY"),
    ) {
        return Ok(Credentials {
            access_key: ak,
            secret_key: sk,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
        });
    }
    // Fallback to ~/.aws/credentials for AWS_PROFILE (or 'default').
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".to_string());
    if let Some(ini) = load_aws_ini("credentials") {
        if let Some(section) = ini.get(&profile) {
            if let (Some(ak), Some(sk)) = (
                section.get("aws_access_key_id"),
                section.get("aws_secret_access_key"),
            ) {
                return Ok(Credentials {
                    access_key: ak.clone(),
                    secret_key: sk.clone(),
                    session_token: section.get("aws_session_token").cloned(),
                });
            }
        }
    }
    bail!(
        "no AWS credentials found — set AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY, \
         or provide ~/.aws/credentials (optionally with AWS_PROFILE)"
    );
}

pub(crate) fn resolve_region() -> String {
    if let Ok(r) = std::env::var("AWS_REGION") {
        return r;
    }
    if let Ok(r) = std::env::var("AWS_DEFAULT_REGION") {
        return r;
    }
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".to_string());
    if let Some(ini) = load_aws_ini("config") {
        // In ~/.aws/config, the default profile section is "[default]";
        // non-default profiles live under "[profile foo]".
        let want = if profile == "default" {
            "default".to_string()
        } else {
            format!("profile {}", profile)
        };
        if let Some(section) = ini.get(&want) {
            if let Some(r) = section.get("region") {
                return r.clone();
            }
        }
    }
    "us-east-1".to_string()
}

/// Returns `(scheme, host, path_style_prefix)`. `path_style_prefix` is
/// `Some("/<bucket>")` when the endpoint uses path-style addressing
/// (MinIO / LocalStack via `AWS_ENDPOINT_URL`), and `None` for the
/// virtual-hosted-style default that real AWS uses.
pub(crate) fn resolve_endpoint(bucket: &str, region: &str) -> Result<(String, String, Option<String>)> {
    if let Ok(ep) = std::env::var("AWS_ENDPOINT_URL") {
        let (scheme, rest) = if let Some(rest) = ep.strip_prefix("http://") {
            ("http".to_string(), rest.to_string())
        } else if let Some(rest) = ep.strip_prefix("https://") {
            ("https".to_string(), rest.to_string())
        } else {
            bail!("AWS_ENDPOINT_URL must start with http:// or https://, got {:?}", ep);
        };
        let host = rest.trim_end_matches('/').to_string();
        return Ok((scheme, host, Some(format!("/{}", bucket))));
    }
    // Virtual-hosted-style: <bucket>.s3.<region>.amazonaws.com, key at /<key>.
    Ok((
        "https".to_string(),
        format!("{}.s3.{}.amazonaws.com", bucket, region),
        None,
    ))
}

fn load_aws_ini(name: &str) -> Option<HashMap<String, HashMap<String, String>>> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home).join(".aws").join(name);
    let text = fs::read_to_string(&path).ok()?;
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if let Some(rest) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = Some(rest.to_string());
            out.entry(rest.to_string()).or_default();
            continue;
        }
        if let (Some(cur), Some(eq)) = (current.as_ref(), t.find('=')) {
            let key = t[..eq].trim().to_string();
            let val = t[eq + 1..].trim().to_string();
            out.entry(cur.clone()).or_default().insert(key, val);
        }
    }
    Some(out)
}

/// Pub wrapper so sibling modules (s3_list) can share one notion of "now".
pub(crate) fn utc_now_pub() -> (u16, u8, u8, u8, u8, u8) {
    utc_now()
}

fn utc_now() -> (u16, u8, u8, u8, u8, u8) {
    // Convert current UNIX timestamp to UTC Y/M/D h:m:s using the civil
    // calendar algorithm from Howard Hinnant.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    (
        y as u16,
        m as u8,
        d as u8,
        (sod / 3600) as u8,
        ((sod % 3600) / 60) as u8,
        (sod % 60) as u8,
    )
}

use crate::dos_time::civil_from_days;
