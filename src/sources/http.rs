//! HTTP(S) archive source — `Read + Seek` backed by HTTP Range requests.
//!
//! Lets `tzip ls` / `tzip cat` / `tzip x` operate against a ZIP sitting at
//! a URL without downloading the whole thing. The ZIP format cooperates:
//! the EOCD lives at the end of the file and every entry's LFH + body is
//! independent, so three Range GETs suffice for a `cat` of one file out
//! of a 100 GB archive.
//!
//! Caches a single 256 KiB-minimum prefetch window so a CDR scan or an
//! entry body read doesn't issue a fresh GET per 8 KiB `Read::read` call.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{self, Read, Seek, SeekFrom};

/// How many bytes to over-fetch past a small read. Big enough to cover the
/// EOCD scan in one GET; small enough that one-off `cat`s don't balloon.
const PREFETCH_MIN: u64 = 256 * 1024;

pub struct HttpSource {
    url: String,
    agent: ureq::Agent,
    len: u64,
    pos: u64,
    cache_start: u64,
    cache: Vec<u8>,
    /// ETag (preferred) or `Last-Modified` from the opening probe,
    /// used by the index cache (Phase 6) as a content-change
    /// fingerprint. `None` when the server emits neither.
    fingerprint: Option<String>,
}

impl HttpSource {
    /// Open an HTTP(S) archive. Probes the server for `Content-Length` and
    /// `Accept-Ranges: bytes`. Falls back to a `Range: bytes=0-0` probe if
    /// the server doesn't answer HEAD well.
    pub fn open(url: &str) -> Result<Self> {
        let agent = super::shared_agent();
        let (len, supports_ranges, fingerprint) = probe_length_and_ranges(&agent, url)?;
        if !supports_ranges {
            bail!(
                "server at {} does not support Range requests \
                 (no Accept-Ranges: bytes header and the probe returned the \
                 whole body instead of a 206 Partial Content)",
                url
            );
        }
        Ok(Self {
            url: url.to_string(),
            agent,
            len,
            pos: 0,
            cache_start: 0,
            cache: Vec::new(),
            fingerprint,
        })
    }

    /// Construct an `HttpSource` with a caller-supplied content length,
    /// skipping the HEAD / Range-0-0 probe. Used by extract / grep inner
    /// workers: the main thread has already opened the archive, so the
    /// length is known and every worker's own probe would be a wasted
    /// round-trip. Range-request support is assumed; if the first body
    /// fetch comes back 200-not-206 we'll fail there with the same
    /// error the probe path would have.
    pub fn open_with_len(url: &str, len: u64) -> Result<Self> {
        let agent = super::shared_agent();
        Ok(Self {
            url: url.to_string(),
            agent,
            len,
            pos: 0,
            cache_start: 0,
            cache: Vec::new(),
            fingerprint: None,
        })
    }

    /// URL this source was opened against. Used by Phase 6 cache
    /// code to derive the archive ID.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// ETag / Last-Modified captured at open time. `None` if the
    /// server didn't emit either (or we came in via
    /// `open_with_len`'s cold path).
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    fn fetch(&mut self, start: u64, min_len: u64) -> io::Result<()> {
        if start >= self.len {
            self.cache_start = start;
            self.cache.clear();
            return Ok(());
        }
        let want = min_len.max(PREFETCH_MIN);
        let end = (start + want).min(self.len);
        let resp = self
            .agent
            .get(&self.url)
            .set("Range", &format!("bytes={}-{}", start, end - 1))
            .call()
            .map_err(ureq_to_io)?;
        if resp.status() != 206 && resp.status() != 200 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unexpected status {} for Range GET", resp.status()),
            ));
        }
        let mut buf: Vec<u8> = Vec::with_capacity((end - start) as usize);
        resp.into_reader().read_to_end(&mut buf)?;
        if buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("empty response body for Range {}-{}", start, end - 1),
            ));
        }
        self.cache_start = start;
        self.cache = buf;
        Ok(())
    }
}

impl Read for HttpSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        // Cache hit?
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
        // Miss: fetch a window starting at pos, then retry from the cache.
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

impl Seek for HttpSource {
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

// ── helpers ───────────────────────────────────────────────────────────────

fn probe_length_and_ranges(
    agent: &ureq::Agent,
    url: &str,
) -> Result<(u64, bool, Option<String>)> {
    // HEAD first.
    match agent.head(url).call() {
        Ok(resp) => {
            let len = resp
                .header("Content-Length")
                .and_then(|s| s.parse::<u64>().ok());
            let accepts = resp
                .header("Accept-Ranges")
                .map(|s| s.to_ascii_lowercase());
            let supports_ranges = accepts.as_deref().map(|s| s.contains("bytes")).unwrap_or(false);
            let fingerprint = pick_fingerprint(&resp);
            if let Some(len) = len {
                return Ok((len, supports_ranges, fingerprint));
            }
        }
        Err(e) => {
            // Some servers refuse HEAD. Fall through to Range probe.
            if let ureq::Error::Status(405, _) | ureq::Error::Status(501, _) = e {
                // fine, try range probe
            } else {
                return Err(anyhow!(e)).with_context(|| format!("HEAD {}", url));
            }
        }
    }

    // Range probe: fetch first byte and read Content-Range: bytes 0-0/LEN.
    let resp = agent
        .get(url)
        .set("Range", "bytes=0-0")
        .call()
        .map_err(|e| anyhow!(e))
        .with_context(|| format!("Range probe GET {}", url))?;
    let status = resp.status();
    if status == 200 {
        // Server ignored our Range header and returned the whole body.
        // That's a "no Range support" signal.
        return Ok((0, false, None));
    }
    if status != 206 {
        bail!("unexpected status {} on Range probe for {}", status, url);
    }
    let fingerprint = pick_fingerprint(&resp);
    let content_range = resp
        .header("Content-Range")
        .ok_or_else(|| anyhow!("206 without Content-Range from {}", url))?
        .to_string();
    // Format: "bytes 0-0/12345" (or "bytes 0-0/*")
    let total = content_range
        .split('/')
        .nth(1)
        .and_then(|t| t.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("could not parse Content-Range {:?}", content_range))?;
    Ok((total, true, fingerprint))
}

/// Pick the strongest "did the archive change" fingerprint the
/// server exposed. ETag is byte-content, `Last-Modified` is a
/// coarser timestamp — either works as a cache-invalidation key,
/// since any change we care about moves both.
fn pick_fingerprint(resp: &ureq::Response) -> Option<String> {
    if let Some(e) = resp.header("ETag") {
        return Some(e.to_string());
    }
    if let Some(m) = resp.header("Last-Modified") {
        return Some(m.to_string());
    }
    None
}

fn ureq_to_io(e: ureq::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}
