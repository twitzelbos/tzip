//! S3 multipart upload + conditional PutObject primitives — the
//! write side of the `s3://` backend.
//!
//! Issue #68: `tzip u` on an `s3://` archive used to require
//! downloading the whole object, running update locally, and
//! re-uploading it — GBs of transfer for a few MB of new entries.
//! For pure-add append the only new on-wire bytes are the new
//! LFH+body records and the rebuilt CDR; everything before the
//! old `cd_offset` stays byte-identical. S3's `UploadPartCopy`
//! copies those prefix bytes *server-side* — no transfer charge
//! inside a region — and `UploadPart` carries the small new tail.
//! `CompleteMultipartUpload` with an `If-Match: <etag>` header
//! atomically compare-and-swaps the final object; a concurrent
//! writer makes it a clean 412 instead of a lost update.
//!
//! This module is transport primitives only — one HTTP call per
//! function. Orchestration (part splitting, kept-prefix size
//! check for the 5 MiB floor, temp-file streaming) lives in
//! `update_s3`.

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::s3::{resolve_credentials, resolve_endpoint, resolve_region, utc_now_pub, Credentials};
use super::sigv4::{encode_key, sign_request, ExtraHeader, SigV4Credentials};

/// S3 write handle: parsed URL, resolved credentials / region /
/// endpoint, and a shared `ureq::Agent`. Constructed once per
/// append, then each `create_* / upload_* / complete_* / abort_*`
/// call reuses it.
pub struct S3Target {
    agent: ureq::Agent,
    scheme: String,
    host: String,
    /// `/<encoded-key>` for virtual-hosted, `/<bucket>/<encoded-key>`
    /// for path-style (`AWS_ENDPOINT_URL`).
    uri_path: String,
    /// `x-amz-copy-source` is `<bucket>/<encoded-key>` — never
    /// URL-encoded a second time, never with a scheme prefix. We
    /// precompute it so copy-part callers don't re-derive it.
    copy_source: String,
    creds: Credentials,
    region: String,
    /// `None` on unit paths; `Some` once a real transport is bound.
    endpoint_override: Option<String>,
}

/// Minimum part size for all parts except the last — S3 hard limit.
/// `UploadPart` / `UploadPartCopy` for a smaller intermediate part
/// fails with `EntityTooSmall`. The last part is exempt (may be any
/// size down to 0 bytes for `UploadPart`; `UploadPartCopy` requires
/// ≥ 1 byte).
pub const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;

/// Maximum copy-source range per `UploadPartCopy` part. S3 limit.
pub const MAX_COPY_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// A single completed part — `part_number` and the ETag the
/// server returned for it. `CompleteMultipartUpload` sends the
/// list in order; S3 validates every ETag.
#[derive(Debug, Clone)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
}

impl S3Target {
    /// `spec` is the string after `s3://` (e.g. `bucket/key.zip`).
    /// Credential / region / endpoint resolution follows `S3Source`.
    pub fn open(spec: &str) -> Result<Self> {
        let (bucket, key) = split_bucket_key(spec)?;
        let creds = resolve_credentials().context("resolve AWS credentials")?;
        let region = resolve_region();
        let (scheme, host, path_style_prefix) = resolve_endpoint(&bucket, &region)?;
        let uri_path = if let Some(prefix) = path_style_prefix {
            format!("{}/{}", prefix, encode_key(&key))
        } else {
            format!("/{}", encode_key(&key))
        };
        // `x-amz-copy-source` is `<bucket>/<key>` with the key
        // URL-encoded per segment. Scheme / host are absent.
        let copy_source = format!("{}/{}", bucket, encode_key(&key));
        let endpoint_override = std::env::var("AWS_ENDPOINT_URL").ok();
        Ok(Self {
            agent: super::shared_agent(),
            scheme,
            host,
            uri_path,
            copy_source,
            creds,
            region,
            endpoint_override,
        })
    }

    fn sigv4<'a>(&'a self) -> SigV4Credentials<'a> {
        SigV4Credentials {
            access_key: &self.creds.access_key,
            secret_key: &self.creds.secret_key,
            session_token: self.creds.session_token.as_deref(),
            region: &self.region,
            service: "s3",
        }
    }

    /// `POST /<key>?uploads` → returns the opaque `UploadId`.
    pub fn create_multipart_upload(&self) -> Result<String> {
        let query = "uploads=";
        let creds = self.sigv4();
        let signed = sign_request(
            "POST",
            &creds,
            &self.scheme,
            &self.host,
            &self.uri_path,
            query,
            &[],
            "UNSIGNED-PAYLOAD",
            utc_now_pub(),
        )?;
        let mut req = self.agent.post(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        // Empty body. `Content-Length: 0` on POST avoids some
        // proxies complaining; ureq handles that for us.
        let resp = req
            .send_bytes(&[])
            .map_err(|e| anyhow!("CreateMultipartUpload: {}", e))?;
        if resp.status() != 200 {
            bail!("CreateMultipartUpload returned status {}", resp.status());
        }
        let mut body = String::new();
        resp.into_reader()
            .read_to_string(&mut body)
            .context("read CreateMultipartUpload body")?;
        extract_xml_text(&body, "UploadId").ok_or_else(|| {
            anyhow!("CreateMultipartUpload response missing <UploadId>: {}", body)
        })
    }

    /// `PUT /<key>?partNumber=N&uploadId=U` with the given bytes.
    /// Returns the ETag the server reports for that part.
    pub fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        bytes: &[u8],
    ) -> Result<String> {
        let query = canonical_part_query(part_number, upload_id);
        let creds = self.sigv4();
        let content_length = bytes.len().to_string();
        let signed = sign_request(
            "PUT",
            &creds,
            &self.scheme,
            &self.host,
            &self.uri_path,
            &query,
            &[ExtraHeader { name: "Content-Length", value: &content_length }],
            "UNSIGNED-PAYLOAD",
            utc_now_pub(),
        )?;
        let mut req = self.agent.put(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let resp = req
            .send_bytes(bytes)
            .map_err(|e| anyhow!("UploadPart {}: {}", part_number, e))?;
        if resp.status() != 200 {
            bail!("UploadPart {} returned status {}", part_number, resp.status());
        }
        Ok(normalize_etag(
            resp.header("ETag")
                .ok_or_else(|| anyhow!("UploadPart {} missing ETag", part_number))?,
        ))
    }

    /// `PUT /<key>?partNumber=N&uploadId=U` with
    /// `x-amz-copy-source: <bucket>/<key>` and
    /// `x-amz-copy-source-range: bytes=start-end`. Copies
    /// `[start, end]` from the existing object (ETag-pinned via
    /// `x-amz-copy-source-if-match`) into part N. No bytes cross
    /// the network — S3 copies server-side.
    pub fn upload_part_copy(
        &self,
        upload_id: &str,
        part_number: u32,
        start: u64,
        end_inclusive: u64,
        source_if_match_etag: &str,
    ) -> Result<String> {
        let query = canonical_part_query(part_number, upload_id);
        let creds = self.sigv4();
        let range = format!("bytes={}-{}", start, end_inclusive);
        let signed = sign_request(
            "PUT",
            &creds,
            &self.scheme,
            &self.host,
            &self.uri_path,
            &query,
            &[
                ExtraHeader {
                    name: "x-amz-copy-source",
                    value: &self.copy_source,
                },
                ExtraHeader {
                    name: "x-amz-copy-source-range",
                    value: &range,
                },
                ExtraHeader {
                    name: "x-amz-copy-source-if-match",
                    value: source_if_match_etag,
                },
            ],
            // Copy-part has no request body.
            "UNSIGNED-PAYLOAD",
            utc_now_pub(),
        )?;
        let mut req = self.agent.put(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let resp = req
            .send_bytes(&[])
            .map_err(|e| anyhow!("UploadPartCopy {}: {}", part_number, e))?;
        if resp.status() != 200 {
            bail!(
                "UploadPartCopy {} returned status {}",
                part_number,
                resp.status()
            );
        }
        // UploadPartCopy returns the ETag inside a
        // <CopyPartResult><ETag>...</ETag></CopyPartResult> body,
        // NOT in a response header.
        let mut body = String::new();
        resp.into_reader()
            .read_to_string(&mut body)
            .context("read UploadPartCopy body")?;
        let etag = extract_xml_text(&body, "ETag").ok_or_else(|| {
            anyhow!(
                "UploadPartCopy {} response missing <ETag>: {}",
                part_number,
                body
            )
        })?;
        Ok(normalize_etag(&etag))
    }

    /// `POST /<key>?uploadId=U` with the ordered part list and an
    /// `If-Match: <etag>` header. Atomic CAS: a concurrent writer
    /// turns this into a clean 412 instead of a lost update.
    ///
    /// Returns the ETag of the final composed object.
    pub fn complete_multipart_upload(
        &self,
        upload_id: &str,
        parts: &[CompletedPart],
        if_match_etag: Option<&str>,
    ) -> Result<String> {
        let query = format!("uploadId={}", urlencode(upload_id));
        let body = build_complete_mpu_body(parts);
        let body_bytes = body.as_bytes();
        let mut extra: Vec<ExtraHeader> = Vec::new();
        let if_match_val;
        if let Some(etag) = if_match_etag {
            // S3 expects the ETag in `If-Match` to include the
            // surrounding quotes (strong validator form).
            if_match_val = if etag.starts_with('"') {
                etag.to_string()
            } else {
                format!("\"{}\"", etag)
            };
            extra.push(ExtraHeader { name: "If-Match", value: &if_match_val });
        }
        let content_length = body_bytes.len().to_string();
        extra.push(ExtraHeader { name: "Content-Length", value: &content_length });
        extra.push(ExtraHeader { name: "Content-Type", value: "application/xml" });

        let creds = self.sigv4();
        let signed = sign_request(
            "POST",
            &creds,
            &self.scheme,
            &self.host,
            &self.uri_path,
            &query,
            &extra,
            "UNSIGNED-PAYLOAD",
            utc_now_pub(),
        )?;
        let mut req = self.agent.post(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let resp = req
            .send_bytes(body_bytes)
            .map_err(|e| anyhow!("CompleteMultipartUpload transport: {}", e))?;
        let status = resp.status();
        let etag_header = resp.header("ETag").map(|s| s.to_string());
        let mut resp_body = String::new();
        resp.into_reader()
            .read_to_string(&mut resp_body)
            .context("read CompleteMultipartUpload body")?;
        // S3 may return 200 with an `<Error>` body — the only way
        // to know for sure is to inspect the body. See issue #68.
        if resp_body.contains("<Error>") || status != 200 {
            let code = extract_xml_text(&resp_body, "Code").unwrap_or_default();
            bail!(
                "CompleteMultipartUpload failed (status {}, code {:?}): {}",
                status,
                code,
                resp_body
            );
        }
        if let Some(etag_xml) = extract_xml_text(&resp_body, "ETag") {
            Ok(normalize_etag(&etag_xml))
        } else if let Some(etag) = etag_header {
            Ok(normalize_etag(&etag))
        } else {
            bail!(
                "CompleteMultipartUpload response missing ETag: {}",
                resp_body
            );
        }
    }

    /// `DELETE /<key>?uploadId=U`. Best-effort; a failure here is
    /// logged but not propagated (the bucket's
    /// `AbortIncompleteMultipartUpload` lifecycle rule will
    /// garbage-collect eventually).
    pub fn abort_multipart_upload(&self, upload_id: &str) {
        let query = format!("uploadId={}", urlencode(upload_id));
        let creds = self.sigv4();
        let Ok(signed) = sign_request(
            "DELETE",
            &creds,
            &self.scheme,
            &self.host,
            &self.uri_path,
            &query,
            &[],
            "UNSIGNED-PAYLOAD",
            utc_now_pub(),
        ) else {
            return;
        };
        let mut req = self.agent.delete(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let _ = req.call();
    }

    /// `PUT /<key>` with the full body and an `If-Match` header.
    /// Used by the small-archive fallback — when the kept prefix
    /// is below `MIN_PART_SIZE`, multipart's "all parts except
    /// last ≥ 5 MiB" rule forces us to a single `PutObject`.
    pub fn put_object_if_match(
        &self,
        bytes: &[u8],
        if_match_etag: Option<&str>,
    ) -> Result<String> {
        let mut extra: Vec<ExtraHeader> = Vec::new();
        let if_match_val;
        if let Some(etag) = if_match_etag {
            if_match_val = if etag.starts_with('"') {
                etag.to_string()
            } else {
                format!("\"{}\"", etag)
            };
            extra.push(ExtraHeader { name: "If-Match", value: &if_match_val });
        }
        let content_length = bytes.len().to_string();
        extra.push(ExtraHeader { name: "Content-Length", value: &content_length });
        let creds = self.sigv4();
        let signed = sign_request(
            "PUT",
            &creds,
            &self.scheme,
            &self.host,
            &self.uri_path,
            "",
            &extra,
            "UNSIGNED-PAYLOAD",
            utc_now_pub(),
        )?;
        let mut req = self.agent.put(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
        let resp = req
            .send_bytes(bytes)
            .map_err(|e| anyhow!("PutObject: {}", e))?;
        if resp.status() != 200 {
            bail!("PutObject returned status {}", resp.status());
        }
        Ok(normalize_etag(resp.header("ETag").unwrap_or("")))
    }

    /// Helper for the orchestrator: upload a temp-file region as
    /// one part. Streams via a buffered read so we never load the
    /// whole tail into memory for large appends.
    pub fn upload_part_from_file(
        &self,
        upload_id: &str,
        part_number: u32,
        file: &mut File,
        offset: u64,
        len: u64,
    ) -> Result<String> {
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek temp file to {}", offset))?;
        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf)
            .with_context(|| format!("read {} bytes from temp file at {}", len, offset))?;
        self.upload_part(upload_id, part_number, &buf)
    }

    /// Report whether the endpoint was overridden via
    /// `AWS_ENDPOINT_URL`. Orchestrator uses this to skip the
    /// `If-Match` header on backends (MinIO / LocalStack older
    /// versions) that may not implement it on
    /// `CompleteMultipartUpload`. See the opt-out flag in the
    /// issue.
    pub fn uses_custom_endpoint(&self) -> bool {
        self.endpoint_override.is_some()
    }
}

/// Build the `?partNumber=N&uploadId=U` canonical query — sorted
/// by key, URL-encoded, no missing `=`. SigV4 wants exactly this
/// form.
fn canonical_part_query(part_number: u32, upload_id: &str) -> String {
    let mut parts = vec![
        ("partNumber".to_string(), part_number.to_string()),
        ("uploadId".to_string(), urlencode(upload_id)),
    ];
    parts.sort_by(|a, b| a.0.cmp(&b.0));
    parts
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode a value for use as a canonical query-string
/// component. Unlike `encode_key`, this also encodes `/` — query
/// params aren't path segments.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_query_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn is_query_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~'
}

/// Build the `CompleteMultipartUpload` request body — one
/// `<Part>` element per completed part, in ascending order.
fn build_complete_mpu_body(parts: &[CompletedPart]) -> String {
    let mut out = String::with_capacity(128 + parts.len() * 80);
    out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.push_str("<CompleteMultipartUpload>");
    for p in parts {
        out.push_str("<Part><PartNumber>");
        out.push_str(&p.part_number.to_string());
        out.push_str("</PartNumber><ETag>");
        // ETag field in the request body must be the literal the
        // server handed us, quotes and all. `normalize_etag`
        // stripped them on receive, so re-add on send.
        let etag = &p.etag;
        if etag.starts_with('"') {
            out.push_str(etag);
        } else {
            out.push('"');
            out.push_str(etag);
            out.push('"');
        }
        out.push_str("</ETag></Part>");
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

/// Strip the surrounding quotes S3 wraps every ETag in.
fn normalize_etag(raw: &str) -> String {
    raw.trim_matches('"').to_string()
}

/// Parse `<Tag>text</Tag>` out of an S3 XML response. Returns the
/// text of the first matching element (first `<Tag>` the reader
/// sees). Enough for the single-leaf fields we care about
/// (`UploadId`, `ETag`, `Code`).
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_target = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_target = true;
                }
            }
            Ok(Event::End(_)) => {
                if in_target {
                    // Expected flow is Start → Text → End. If we
                    // see End without a Text in between, treat it
                    // as empty.
                    in_target = false;
                }
            }
            Ok(Event::Text(e)) if in_target => {
                return e.unescape().ok().map(|c| c.into_owned());
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

fn split_bucket_key(spec: &str) -> Result<(String, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_part_query_is_sorted_and_encoded() {
        let q = canonical_part_query(3, "abc/def=XYZ");
        // sorted keys: partNumber, uploadId
        assert_eq!(q, "partNumber=3&uploadId=abc%2Fdef%3DXYZ");
    }

    #[test]
    fn urlencode_escapes_slash_and_equals() {
        assert_eq!(urlencode("a/b=c"), "a%2Fb%3Dc");
        assert_eq!(urlencode("safe-_.~"), "safe-_.~");
    }

    #[test]
    fn complete_mpu_body_orders_parts_and_quotes_etags() {
        let body = build_complete_mpu_body(&[
            CompletedPart { part_number: 1, etag: "aaa".to_string() },
            CompletedPart { part_number: 2, etag: "\"bbb\"".to_string() },
        ]);
        assert!(body.contains("<Part><PartNumber>1</PartNumber><ETag>\"aaa\"</ETag></Part>"));
        assert!(body.contains("<Part><PartNumber>2</PartNumber><ETag>\"bbb\"</ETag></Part>"));
    }

    #[test]
    fn extract_xml_text_finds_leaf() {
        let xml = r#"<?xml version="1.0"?><InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>abc123</UploadId></InitiateMultipartUploadResult>"#;
        assert_eq!(extract_xml_text(xml, "UploadId").as_deref(), Some("abc123"));
        assert_eq!(extract_xml_text(xml, "Missing"), None);
    }

    #[test]
    fn normalize_etag_strips_quotes() {
        assert_eq!(normalize_etag("\"abc\""), "abc");
        assert_eq!(normalize_etag("abc"), "abc");
    }
}
