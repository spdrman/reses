//! AWS Signature Version 4, just enough for S3 with header-based auth.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use super::Credentials;

/// SHA-256 of an empty body, which every GET and DELETE sends.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const AMZ_DATE: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

/// What gets signed. `path` is the canonical URI, already encoded; `query` and `headers`
/// are raw and get encoded and normalised here.
pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256: &'a str,
}

/// The result of signing, with every intermediate step kept so tests can check it.
pub struct Signature {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    pub authorization: String,
    /// Headers to send on top of `Request::headers`: date, payload hash, token, auth.
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signature")
            .field("signature", &self.signature)
            .finish_non_exhaustive()
    }
}

/// S3's URI encoding: every byte but `A-Za-z0-9-_.~` becomes `%XX`, and `/` too unless
/// `keep_slash` (object paths keep it, query values don't).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The canonical query string: encoded pairs sorted by key, joined with `&`.
pub fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Trim a header value and squeeze runs of spaces, as the canonical form wants.
fn canonical_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn sign(
    req: &Request<'_>,
    creds: &Credentials,
    region: &str,
    service: &str,
    now: OffsetDateTime,
) -> Signature {
    let now = now.to_offset(time::UtcOffset::UTC);
    let amz_date = now.format(AMZ_DATE).expect("fixed format");
    let date = &amz_date[..8];

    let mut added = vec![
        ("x-amz-date".to_string(), amz_date.clone()),
        (
            "x-amz-content-sha256".to_string(),
            req.payload_sha256.to_string(),
        ),
    ];
    if let Some(token) = &creds.session_token {
        added.push(("x-amz-security-token".to_string(), token.clone()));
    }

    let mut signed: Vec<(String, String)> = vec![("host".to_string(), req.host.to_string())];
    signed.extend(
        req.headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), canonical_value(v))),
    );
    signed.extend(added.iter().cloned());
    signed.sort();

    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = signed
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.path,
        canonical_query(req.query),
        canonical_headers,
        signed_headers,
        req.payload_sha256
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let mut key = hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date);
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex::encode(hmac(&key, &string_to_sign));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",
        creds.access_key_id
    );

    let mut headers = added;
    headers.push(("authorization".to_string(), authorization.clone()));
    Signature {
        canonical_request,
        string_to_sign,
        signature,
        authorization,
        headers,
    }
}
