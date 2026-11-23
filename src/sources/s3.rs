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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::sigv4::{encode_key, sign_get, ExtraHeader, SigV4Credentials};

const PREFETCH_MIN: u64 = 256 * 1024;

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
}

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

        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .user_agent(&format!("tzip/{}", env!("CARGO_PKG_VERSION")))
            .build();

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
        };
        src.probe_len()?;
        Ok(src)
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
        let want = min_len.max(PREFETCH_MIN);
        let end = (start + want).min(self.len);
        let range = format!("bytes={}-{}", start, end - 1);
        let signed = self
            .sign(&[ExtraHeader { name: "Range", value: &range }])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let mut req = self.agent.get(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        if resp.status() != 206 && resp.status() != 200 {
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
        Ok(())
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

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
