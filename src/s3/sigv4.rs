//! AWS Signature Version 4, just enough for S3 with header-based auth.

use time::OffsetDateTime;

use super::Credentials;

/// SHA-256 of an empty body, which every GET and DELETE sends.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

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
pub fn uri_encode(_s: &str, _keep_slash: bool) -> String {
    todo!()
}

/// The canonical query string: encoded pairs sorted by key, joined with `&`.
pub fn canonical_query(_query: &[(&str, &str)]) -> String {
    todo!()
}

pub fn sha256_hex(_data: &[u8]) -> String {
    todo!()
}

pub fn sign(
    _req: &Request<'_>,
    _creds: &Credentials,
    _region: &str,
    _service: &str,
    _now: OffsetDateTime,
) -> Signature {
    todo!()
}
