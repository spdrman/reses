//! Parsing the XML bodies S3 answers with.

use percent_encoding::percent_decode_str;
use quick_xml::events::Event;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Bucket, Listing, ObjectInfo, S3Error};

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

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListAllMyBucketsResult {
    #[serde(default)]
    buckets: BucketList,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct BucketList {
    #[serde(default)]
    bucket: Vec<BucketEl>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BucketEl {
    name: String,
    creation_date: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListBucketResult {
    #[serde(default)]
    contents: Vec<ContentsEl>,
    #[serde(default)]
    common_prefixes: Vec<PrefixEl>,
    next_continuation_token: Option<String>,
    is_truncated: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContentsEl {
    key: String,
    size: u64,
    last_modified: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PrefixEl {
    prefix: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ErrorEl {
    code: String,
    #[serde(default)]
    message: String,
    region: Option<String>,
    endpoint: Option<String>,
}

/// Name of the document's root element, so one kind of document is never read as another.
fn root_name(body: &[u8]) -> Result<String, S3Error> {
    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                return Ok(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
            }
            Ok(Event::Eof) => return Err(S3Error::Parse("empty XML document".into())),
            Ok(_) => buf.clear(),
            Err(e) => return Err(S3Error::Parse(format!("bad XML: {e}"))),
        }
    }
}

fn parse<T: DeserializeOwned>(body: &[u8], root: &str) -> Result<T, S3Error> {
    let found = root_name(body)?;
    if found != root {
        return Err(S3Error::Parse(format!("expected <{root}>, got <{found}>")));
    }
    let text = std::str::from_utf8(body).map_err(|_| S3Error::Parse("XML is not UTF-8".into()))?;
    quick_xml::de::from_str(text).map_err(|e| S3Error::Parse(format!("bad <{root}>: {e}")))
}

/// S3 timestamps are RFC 3339. One that doesn't parse is dropped rather than failing a listing.
fn timestamp(s: Option<&str>) -> Option<OffsetDateTime> {
    s.and_then(|s| OffsetDateTime::parse(s.trim(), &Rfc3339).ok())
}

/// Undo `encoding-type=url`, which S3 applies form-style: `+` is a space, `%2B` a plus.
fn url_decode(s: &str) -> Result<String, S3Error> {
    percent_decode_str(&s.replace('+', " "))
        .decode_utf8()
        .map(|c| c.into_owned())
        .map_err(|_| S3Error::Parse(format!("key is not UTF-8 once decoded: {s}")))
}

pub fn parse_buckets(body: &[u8]) -> Result<Vec<Bucket>, S3Error> {
    let doc: ListAllMyBucketsResult = parse(body, "ListAllMyBucketsResult")?;
    Ok(doc
        .buckets
        .bucket
        .into_iter()
        .map(|b| Bucket {
            created: timestamp(b.creation_date.as_deref()),
            name: b.name,
        })
        .collect())
}

/// A ListObjectsV2 page. `url_encoded` says the request asked for `encoding-type=url`,
/// so keys and prefixes need decoding.
pub fn parse_listing(body: &[u8], url_encoded: bool) -> Result<Listing, S3Error> {
    let doc: ListBucketResult = parse(body, "ListBucketResult")?;
    let decode = |s: String| if url_encoded { url_decode(&s) } else { Ok(s) };
    let mut listing = Listing::default();
    for p in doc.common_prefixes {
        listing.prefixes.push(decode(p.prefix)?);
    }
    for c in doc.contents {
        listing.objects.push(ObjectInfo {
            last_modified: timestamp(c.last_modified.as_deref()),
            key: decode(c.key)?,
            size: c.size,
        });
    }
    // A truncated page always carries a token; trust the flag when both are there.
    listing.next_token = match doc.is_truncated {
        Some(false) => None,
        _ => doc.next_continuation_token.filter(|t| !t.is_empty()),
    };
    Ok(listing)
}

pub fn parse_error(body: &[u8]) -> Result<ErrorDoc, S3Error> {
    let doc: ErrorEl = parse(body, "Error")?;
    Ok(ErrorDoc {
        code: doc.code,
        message: doc.message,
        region: doc.region.filter(|r| !r.is_empty()),
        endpoint: doc.endpoint.filter(|e| !e.is_empty()),
    })
}
