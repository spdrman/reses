//! Parsing the XML bodies S3 answers with.

use super::{Bucket, Listing, S3Error};

/// An S3 `<Error>` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorDoc {
    pub code: String,
    pub message: String,
    /// Set on `AuthorizationHeaderMalformed` and some redirects.
    pub region: Option<String>,
    /// Set on `PermanentRedirect`.
    pub endpoint: Option<String>,
}

pub fn parse_buckets(_body: &[u8]) -> Result<Vec<Bucket>, S3Error> {
    todo!()
}

/// A ListObjectsV2 page. `url_encoded` says the request asked for `encoding-type=url`,
/// so keys and prefixes need decoding.
pub fn parse_listing(_body: &[u8], _url_encoded: bool) -> Result<Listing, S3Error> {
    todo!()
}

pub fn parse_error(_body: &[u8]) -> Result<ErrorDoc, S3Error> {
    todo!()
}
