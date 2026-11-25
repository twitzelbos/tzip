//! S3 bucket-globbing: resolve `s3://bucket/prefix/*.zip` to a concrete
//! list of `s3://bucket/key` URLs via the ListObjectsV2 operation.
//!
//! Reuses `sources::s3`'s credential + region + endpoint resolution so the
//! user-visible knobs (env vars, AWS_PROFILE, AWS_ENDPOINT_URL) behave
//! identically to `tzip ls s3://bucket/key.zip`. Signs the ListObjectsV2
//! GET via `sources::sigv4::sign_get` with a canonical query string.

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::s3::{resolve_credentials, resolve_endpoint, resolve_region, Credentials};
use super::sigv4::{sign_get, ExtraHeader, SigV4Credentials};

/// Returns `true` if `spec` (the string after `s3://`) contains a `*` or
/// `?` in the *key* portion. The bucket portion never contains wildcards.
pub fn has_glob(spec: &str) -> bool {
    let after_bucket = spec.splitn(2, '/').nth(1).unwrap_or("");
    after_bucket.contains('*') || after_bucket.contains('?')
}

/// Expand `s3://bucket/prefix/*.zip` into a list of concrete `s3://` URLs.
/// `spec` is the string after the `s3://` prefix.
pub fn expand(spec: &str) -> Result<Vec<String>> {
    let (bucket, pattern) = split_bucket_pattern(spec)?;
    let prefix = literal_prefix_of(&pattern);

    let creds = resolve_credentials().context("resolve AWS credentials")?;
    let region = resolve_region();
    let (scheme, host, path_style_prefix) = resolve_endpoint(&bucket, &region)?;
    let agent = super::shared_agent();

    let mut keys: Vec<String> = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let (page, next) = list_page(
            &agent, &creds, &region, &scheme, &host, &path_style_prefix, &prefix, continuation.as_deref(),
        )?;
        for k in page {
            if glob_match(&pattern, &k) {
                keys.push(format!("s3://{}/{}", bucket, k));
            }
        }
        match next {
            Some(t) => continuation = Some(t),
            None => break,
        }
    }
    Ok(keys)
}

fn list_page(
    agent: &ureq::Agent,
    creds: &Credentials,
    region: &str,
    scheme: &str,
    host: &str,
    path_style_prefix: &Option<String>,
    prefix: &str,
    continuation: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    // Build canonical query string, alphabetically sorted by key (SigV4).
    // Fields we always send: list-type, max-keys, prefix.
    // Optional: continuation-token.
    let mut parts: Vec<(String, String)> = Vec::new();
    if let Some(t) = continuation {
        parts.push(("continuation-token".to_string(), uri_encode(t)));
    }
    parts.push(("list-type".to_string(), "2".to_string()));
    parts.push(("max-keys".to_string(), "1000".to_string()));
    parts.push(("prefix".to_string(), uri_encode(prefix)));
    parts.sort_by(|a, b| a.0.cmp(&b.0));
    let query: String = parts
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // URI path: `/` for virtual-hosted; `/<bucket>` for path-style.
    let uri_path = path_style_prefix.clone().unwrap_or_else(|| "/".to_string());
    let uri_path = if uri_path == "/" { "/".to_string() } else { uri_path };

    let sigv4_creds = SigV4Credentials {
        access_key: &creds.access_key,
        secret_key: &creds.secret_key,
        session_token: creds.session_token.as_deref(),
        region,
        service: "s3",
    };
    let now = crate::sources::s3::utc_now_pub();
    let signed = sign_get(&sigv4_creds, scheme, host, &uri_path, &query, &[] as &[ExtraHeader], now)?;

    let mut req = agent.get(&signed.url);
    for (k, v) in &signed.headers {
        req = req.set(k, v);
    }
    let resp = req
        .call()
        .map_err(|e| anyhow!("S3 ListObjectsV2 failed: {}", e))?;
    if resp.status() != 200 {
        bail!("S3 ListObjectsV2 returned status {}", resp.status());
    }
    let body = resp.into_string().context("read S3 list body")?;
    parse_list_response(&body)
}

fn parse_list_response(xml: &str) -> Result<(Vec<String>, Option<String>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut keys: Vec<String> = Vec::new();
    let mut next_token: Option<String> = None;
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                path.push(name);
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().into_owned();
                let joined: String = path.join("/");
                match joined.as_str() {
                    "ListBucketResult/Contents/Key" => keys.push(text),
                    "ListBucketResult/NextContinuationToken" => next_token = Some(text),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("S3 XML parse error at {}: {}", reader.buffer_position(), e),
            _ => {}
        }
        buf.clear();
    }
    Ok((keys, next_token))
}

fn split_bucket_pattern(spec: &str) -> Result<(String, String)> {
    let (bucket, pattern) = spec
        .split_once('/')
        .ok_or_else(|| anyhow!("s3 glob must be s3://bucket/key-pattern — got s3://{}", spec))?;
    if bucket.is_empty() {
        bail!("empty bucket in s3://{}", spec);
    }
    Ok((bucket.to_string(), pattern.to_string()))
}

/// Literal prefix before the first `*` or `?` — what we tell S3 to filter by.
fn literal_prefix_of(pattern: &str) -> String {
    let stop = pattern
        .find(|c| c == '*' || c == '?')
        .unwrap_or(pattern.len());
    pattern[..stop].to_string()
}

/// Small `*`/`?` glob match, anchored on both ends. Mirrors
/// `walker::glob_match` so include/exclude behavior is identical.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn recur(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if recur(&p[1..], t) {
                    return true;
                }
                if let Some((_, rest)) = t.split_first() {
                    return recur(p, rest);
                }
                false
            }
            (Some(b'?'), Some(_)) => recur(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => recur(&p[1..], &t[1..]),
            _ => false,
        }
    }
    recur(pattern.as_bytes(), text.as_bytes())
}

/// SigV4-safe URI encoding of a query parameter value. Matches the
/// unreserved-char set from the SigV4 spec; same encoding used in
/// `sigv4::encode_key`, but values (not keys) need percent-escaping too.
fn uri_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() + 8);
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefix_strips_at_first_wildcard() {
        assert_eq!(literal_prefix_of("logs/2026-11-*.zip"), "logs/2026-11-");
        assert_eq!(literal_prefix_of("*.zip"), "");
        assert_eq!(literal_prefix_of("plain"), "plain");
        assert_eq!(literal_prefix_of("x?y"), "x");
    }

    #[test]
    fn has_glob_detects_wildcards_in_key_only() {
        assert!(has_glob("bucket/logs/*.zip"));
        assert!(has_glob("bucket/x?y.zip"));
        assert!(!has_glob("bucket/plain.zip"));
    }

    #[test]
    fn glob_match_basic() {
        assert!(glob_match("*.zip", "foo.zip"));
        assert!(glob_match("logs/*.zip", "logs/a.zip"));
        assert!(!glob_match("logs/*.zip", "other/a.zip"));
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("?.txt", "ab.txt"));
    }

    #[test]
    fn parse_list_response_extracts_keys_and_token() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name>
  <Prefix>logs/</Prefix>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>next-token-xyz</NextContinuationToken>
  <Contents>
    <Key>logs/a.zip</Key>
    <Size>100</Size>
  </Contents>
  <Contents>
    <Key>logs/b.zip</Key>
    <Size>200</Size>
  </Contents>
</ListBucketResult>"#;
        let (keys, token) = parse_list_response(xml).unwrap();
        assert_eq!(keys, vec!["logs/a.zip".to_string(), "logs/b.zip".to_string()]);
        assert_eq!(token.as_deref(), Some("next-token-xyz"));
    }
}
