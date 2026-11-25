//! Hand-rolled AWS Signature Version 4 request signer for the GetObject
//! operation against S3. Keeps the dependency tree small: just `sha2` for
//! SHA-256 (paired with the existing `hmac` crate used by the AES path).
//!
//! References:
//! - AWS SigV4 spec: <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>
//! - S3 uses *single*-URI-encoding (unlike most AWS services which
//!   double-encode). We pass path segments through once and that matches
//!   what boto3 / aws-sdk-s3 send.

use anyhow::Result;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Credentials used for signing. Session token is optional (used by
/// temporary creds from STS / instance roles).
pub struct SigV4Credentials<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub session_token: Option<&'a str>,
    pub region: &'a str,
    pub service: &'a str, // "s3"
}

/// A header the caller wants folded into the signed request.
pub struct ExtraHeader<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

/// Fully signed GET request: `url` goes straight into `ureq.get(url)`,
/// `headers` are set verbatim before `.call()`.
pub struct SignedRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Sign a request. Works for every method the S3 API uses — GET,
/// PUT (including `UploadPartCopy`'s `x-amz-copy-source` header),
/// POST (`CreateMultipartUpload` / `CompleteMultipartUpload`),
/// DELETE (`AbortMultipartUpload`), HEAD.
///
/// Arguments:
/// - `method`: ASCII uppercase (`"GET"`, `"PUT"`, `"POST"`, `"DELETE"`).
/// - `host`: `<bucket>.s3.<region>.amazonaws.com` for virtual-hosted
///   style, or the endpoint host for path-style via `AWS_ENDPOINT_URL`.
/// - `path`: `/<encoded-key>`; the caller passed it through
///   `encode_key`.
/// - `query`: canonical query — sorted by key, URL-encoded per
///   SigV4 rules, keys without values rendered as `key=` (empty).
///   May be empty.
/// - `payload_hash`: lowercase hex SHA-256 of the body, or the
///   literal string `"UNSIGNED-PAYLOAD"`. S3 PUT / POST accept the
///   latter over HTTPS.
/// - `now`: injected for deterministic tests.
pub fn sign_request(
    method: &str,
    creds: &SigV4Credentials,
    scheme: &str,
    host: &str,
    path: &str,
    query: &str,
    extra_headers: &[ExtraHeader],
    payload_hash: &str,
    now: (u16, u8, u8, u8, u8, u8),
) -> Result<SignedRequest> {
    let (yyyy, mm, dd, h, m, s) = now;
    let amz_date = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        yyyy, mm, dd, h, m, s
    );
    let date_only = format!("{:04}{:02}{:02}", yyyy, mm, dd);

    // Collect headers that go into the signature (canonical + signed lists).
    // Required: host, x-amz-content-sha256, x-amz-date.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(4 + extra_headers.len());
    headers.push(("host".to_string(), host.to_string()));
    headers.push(("x-amz-content-sha256".to_string(), payload_hash.to_string()));
    headers.push(("x-amz-date".to_string(), amz_date.clone()));
    if let Some(token) = creds.session_token {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    for eh in extra_headers {
        headers.push((eh.name.to_ascii_lowercase(), eh.value.to_string()));
    }
    // Sort lexicographically by header name (lowercase).
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
        .collect();
    let signed_headers: String = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, path, query, canonical_headers, signed_headers, payload_hash
    );
    let hashed_canonical = hex_sha256(canonical_request.as_bytes());

    let scope = format!("{}/{}/{}/aws4_request", date_only, creds.region, creds.service);
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", amz_date, scope, hashed_canonical);

    let k_date = hmac_sha256(format!("AWS4{}", creds.secret_key).as_bytes(), date_only.as_bytes());
    let k_region = hmac_sha256(&k_date, creds.region.as_bytes());
    let k_service = hmac_sha256(&k_region, creds.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature_bytes = hmac_sha256(&k_signing, string_to_sign.as_bytes());
    let signature = to_hex(&signature_bytes);

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        creds.access_key, scope, signed_headers, signature
    );

    let url = if query.is_empty() {
        format!("{}://{}{}", scheme, host, path)
    } else {
        format!("{}://{}{}?{}", scheme, host, path, query)
    };

    let mut out_headers: Vec<(String, String)> = Vec::with_capacity(headers.len() + 1);
    // Skip "host" — ureq sets it from the URL.
    for (k, v) in headers.iter().filter(|(k, _)| k != "host") {
        out_headers.push((k.clone(), v.clone()));
    }
    out_headers.push(("Authorization".to_string(), authorization));

    Ok(SignedRequest { url, headers: out_headers })
}

/// Sign a GET. Thin wrapper over `sign_request` for the common
/// read path. Uses `UNSIGNED-PAYLOAD` for the body hash (no body).
pub fn sign_get(
    creds: &SigV4Credentials,
    scheme: &str,
    host: &str,
    path: &str,
    query: &str,
    extra_headers: &[ExtraHeader],
    now: (u16, u8, u8, u8, u8, u8),
) -> Result<SignedRequest> {
    sign_request(
        "GET",
        creds,
        scheme,
        host,
        path,
        query,
        extra_headers,
        "UNSIGNED-PAYLOAD",
        now,
    )
}

// ── primitives ────────────────────────────────────────────────────────────

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    to_hex(&h.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Percent-encode an S3 object key per segment for use as the URI path.
/// Keeps `/` unescaped (segment separator), escapes everything that AWS
/// requires. S3 uses single encoding (not the double-encoding most AWS
/// services do), so the output of this feeds both the canonical URI and
/// the request URL.
pub fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 8);
    for &b in key.as_bytes() {
        if is_unreserved(b) || b == b'/' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

fn is_unreserved(b: u8) -> bool {
    (b'A'..=b'Z').contains(&b)
        || (b'a'..=b'z').contains(&b)
        || (b'0'..=b'9').contains(&b)
        || b == b'-'
        || b == b'_'
        || b == b'.'
        || b == b'~'
}

// ── unit tests: AWS SigV4 reference vectors ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS SigV4 `get-vanilla` test vector:
    ///   https://docs.aws.amazon.com/general/latest/gr/sigv4_testsuite.html
    ///
    /// Access key = "AKIDEXAMPLE"
    /// Secret key = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"
    /// Region = "us-east-1", Service = "service"
    /// Date = 2015-08-30T12:36:00Z
    /// Request: GET / HTTP/1.1
    ///          Host: example.amazonaws.com
    ///
    /// Expected signature (from AWS docs):
    ///   5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31
    #[test]
    fn sigv4_get_vanilla() {
        let creds = SigV4Credentials {
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token: None,
            region: "us-east-1",
            service: "service",
        };
        // Build a canonical request matching get-vanilla: no extra headers,
        // empty query, path "/", payload-hash = empty SHA256 (NOT
        // UNSIGNED-PAYLOAD for this reference).
        let amz_date = "20150830T123600Z";
        let date_only = "20150830";
        let empty_hash = hex_sha256(b"");
        let canonical = format!(
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:{}\n\nhost;x-amz-date\n{}",
            amz_date, empty_hash
        );
        let scope = format!("{}/us-east-1/service/aws4_request", date_only);
        let sts = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            hex_sha256(canonical.as_bytes())
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", creds.secret_key).as_bytes(),
            date_only.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, creds.region.as_bytes());
        let k_service = hmac_sha256(&k_region, creds.service.as_bytes());
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let sig = to_hex(&hmac_sha256(&k_signing, sts.as_bytes()));
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// Spot-check our sign_get for an S3-style GET with a Range header.
    /// We can't check the signature against a fixed vector (AWS doesn't
    /// publish one with Range), so instead we verify:
    ///  - The returned URL is well-formed.
    ///  - The Authorization header has the right prefix, lists x-amz-* +
    ///    host + range in SignedHeaders (lowercase, sorted), and has a
    ///    64-hex-char signature.
    #[test]
    fn sigv4_get_with_range_well_formed() {
        let creds = SigV4Credentials {
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token: None,
            region: "us-east-1",
            service: "s3",
        };
        let signed = sign_get(
            &creds,
            "https",
            "mybucket.s3.us-east-1.amazonaws.com",
            "/my/key.zip",
            "",
            &[ExtraHeader { name: "Range", value: "bytes=0-1023" }],
            (2026, 11, 23, 15, 30, 0),
        )
        .expect("signing failed");

        assert_eq!(
            signed.url,
            "https://mybucket.s3.us-east-1.amazonaws.com/my/key.zip"
        );
        let auth = signed
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.as_str())
            .expect("no Authorization header");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 "));
        // SignedHeaders list must include host and range, in sorted lowercase.
        let signed_headers_section = auth
            .split("SignedHeaders=")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .expect("no SignedHeaders");
        assert!(signed_headers_section.contains("host"));
        assert!(signed_headers_section.contains("range"));
        assert!(signed_headers_section.contains("x-amz-content-sha256"));
        assert!(signed_headers_section.contains("x-amz-date"));
        // Signature is 64 lowercase hex chars.
        let sig_section = auth
            .split("Signature=")
            .nth(1)
            .expect("no Signature=");
        assert_eq!(sig_section.len(), 64);
        assert!(sig_section.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn session_token_propagates_to_headers() {
        let creds = SigV4Credentials {
            access_key: "ASIA...",
            secret_key: "secret",
            session_token: Some("FakeSessionToken"),
            region: "us-west-2",
            service: "s3",
        };
        let signed = sign_get(
            &creds, "https", "b.s3.us-west-2.amazonaws.com", "/k", "", &[],
            (2026, 1, 1, 0, 0, 0),
        )
        .unwrap();
        let has_token = signed
            .headers
            .iter()
            .any(|(k, v)| k == "x-amz-security-token" && v == "FakeSessionToken");
        assert!(has_token, "session token must be sent as x-amz-security-token");
    }

    #[test]
    fn encode_key_keeps_slashes_escapes_spaces() {
        assert_eq!(encode_key("foo/bar.txt"), "foo/bar.txt");
        assert_eq!(encode_key("foo bar/baz+qux.txt"), "foo%20bar/baz%2Bqux.txt");
        assert_eq!(encode_key("safe-_.~"), "safe-_.~");
    }
}
