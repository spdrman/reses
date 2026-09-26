//! S3 for reses: the `Store` trait the screens talk to, the SDK-backed `S3Client`, and the
//! in-memory `MemoryStore` the tests use.
//!
//! I keep the screens on a small synchronous trait so they never see async code or SDK types,
//! and so every TUI test can run offline against `MemoryStore`. The real work (signing,
//! retries, credentials, XML) is aws-sdk-s3's; `client.rs` only adapts it to this trait and
//! adds the few guarantees reses makes on top.

mod client;

pub use client::S3Client;

/// SES refuses messages over 40 MB, so a whole-object read stops a little past that.
pub const MAX_GET_BYTES: u64 = 41 * 1024 * 1024;
/// What a ranged read will take on top of the bytes it asked for, before it stops reading.
pub const RANGE_SLACK: u64 = 4096;

use std::collections::BTreeMap;
use std::sync::Mutex;

use time::OffsetDateTime;

/// A fixed set of keys, for a client that isn't built from a profile (MinIO and the tests).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// I show the key id only. The secret and token stay out, so a debug log can't leak them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// One bucket from ListBuckets. `created` is `None` when the server didn't say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub name: String,
    pub created: Option<OffsetDateTime>,
}

/// One object in a listing: its raw key (never escaped for display), its size and its date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<OffsetDateTime>,
}

/// One page of a ListObjectsV2 call. With a delimiter, `prefixes` holds the "folders".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub prefixes: Vec<String>,
    pub objects: Vec<ObjectInfo>,
    pub next_token: Option<String>,
}

impl Listing {
    /// The token to ask for the next page with, given the one this page was fetched with.
    /// `None` when there are no more pages, and also when the server handed back an empty
    /// token or the same token again, which would otherwise page forever.
    pub fn next_page(&self, sent: Option<&str>) -> Option<&str> {
        self.next_token
            .as_deref()
            .filter(|t| !t.is_empty() && Some(*t) != sent)
    }
}

/// Whether `region` looks like a region name (`us-east-1`, `eu-west-2`, a MinIO region). The
/// region goes into a hostname and the signing scope, so anything else is refused.
pub fn valid_region(region: &str) -> bool {
    (1..=63).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !region.starts_with('-')
        && !region.ends_with('-')
}

/// The size half of a `TooLarge` message, which has to read sensibly when the size is unknown.
fn describe_size(size: &Option<u64>) -> String {
    match size {
        Some(n) => format!("{n} bytes"),
        None => "size unknown".to_string(),
    }
}

/// Everything a `Store` call can fail with. The variants are plain data, with no SDK types, so
/// the screens can match on them and `MemoryStore` can produce the same ones.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum S3Error {
    /// S3 answered with an error document.
    #[error("{code} ({status}): {message}")]
    Service {
        status: u16,
        code: String,
        message: String,
    },
    /// No answer from S3 at all: DNS, connect, TLS, a timeout, or credentials that didn't resolve.
    #[error("network error: {0}")]
    Transport(String),
    /// S3 answered with something the client couldn't read.
    #[error("unexpected response: {0}")]
    Parse(String),
    /// The object is bigger than the client will read in one go. `size` is the object's
    /// length when the server said it, and `limit` the most the client reads.
    #[error("the object is too large to open ({}, the limit is {limit} bytes)", describe_size(.size))]
    TooLarge { size: Option<u64>, limit: u64 },
    /// An empty object key, which S3 would read as the bucket itself.
    #[error("an object key can't be empty")]
    EmptyKey,
}

impl S3Error {
    /// Whether S3 answered 404, so a screen can say the message no longer exists rather than
    /// show a raw error.
    pub fn is_not_found(&self) -> bool {
        matches!(self, S3Error::Service { status: 404, .. })
    }
}

/// The handful of S3 calls reses makes, as plain blocking methods. It's `Send + Sync` because
/// the screens share one store across their worker threads.
pub trait Store: Send + Sync {
    /// Every bucket the credentials can see.
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error>;
    /// One page of keys under `prefix`. `delimiter` groups keys into folders ("/").
    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error>;
    /// Bytes `start..=end` of an object (clamped to its length).
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error>;
    /// A whole object.
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error>;
    /// Delete one object. A key that is already gone still counts as success.
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error>;
    /// The region the store learned for `bucket` from a redirect, if it did. The default
    /// is `None`, for stores that have no regions.
    fn bucket_region(&self, bucket: &str) -> Option<String> {
        let _ = bucket;
        None
    }
}

/// One bucket's objects in `MemoryStore`, by key: the bytes and a last-modified time.
type Objects = BTreeMap<String, (Vec<u8>, OffsetDateTime)>;

/// In-memory store for tests. Page size is small on purpose so paging gets exercised. Keys sit
/// in a `BTreeMap`, which gives me S3's lexical listing order for free.
pub struct MemoryStore {
    buckets: Mutex<BTreeMap<String, Objects>>,
    page_size: usize,
}

impl Default for MemoryStore {
    /// The same as [`MemoryStore::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// An empty store with no buckets, listing three entries a page.
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(BTreeMap::new()),
            page_size: 3,
        }
    }

    /// List `n` entries a page instead. Zero becomes one, since an empty page would never end.
    pub fn with_page_size(mut self, n: usize) -> Self {
        self.page_size = n.max(1);
        self
    }

    /// Store an object, creating the bucket if needed. Every object gets the Unix epoch as its
    /// date, so tests that show dates stay stable.
    pub fn put(&self, bucket: &str, key: &str, data: &[u8]) {
        let mut b = self.buckets.lock().unwrap();
        b.entry(bucket.to_string())
            .or_default()
            .insert(key.to_string(), (data.to_vec(), OffsetDateTime::UNIX_EPOCH));
    }

    /// Add an empty bucket, or leave an existing one alone.
    pub fn create_bucket(&self, bucket: &str) {
        self.buckets
            .lock()
            .unwrap()
            .entry(bucket.to_string())
            .or_default();
    }

    /// Whether the object exists, so a test can check that a delete really happened.
    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        self.buckets
            .lock()
            .unwrap()
            .get(bucket)
            .is_some_and(|b| b.contains_key(key))
    }

    /// The 404 S3 gives for a missing bucket.
    fn no_bucket(bucket: &str) -> S3Error {
        S3Error::Service {
            status: 404,
            code: "NoSuchBucket".into(),
            message: format!("bucket {bucket} does not exist"),
        }
    }

    /// The 404 S3 gives for a missing key.
    fn no_key(key: &str) -> S3Error {
        S3Error::Service {
            status: 404,
            code: "NoSuchKey".into(),
            message: format!("key {key} does not exist"),
        }
    }
}

impl Store for MemoryStore {
    /// Every bucket, by name, with no creation date.
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        Ok(self
            .buckets
            .lock()
            .unwrap()
            .keys()
            .map(|name| Bucket {
                name: name.clone(),
                created: None,
            })
            .collect())
    }

    /// One page, the way ListObjectsV2 builds it: keys under the prefix in order, anything past a
    /// delimiter folded into one folder entry, and the token being the last entry returned.
    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error> {
        let buckets = self.buckets.lock().unwrap();
        let objects = buckets.get(bucket).ok_or_else(|| Self::no_bucket(bucket))?;

        // Flatten to the ordered entries S3 would return, folders collapsed, then page.
        let mut entries: Vec<(String, Option<ObjectInfo>)> = Vec::new();
        for (key, (data, modified)) in objects.range(prefix.to_string()..) {
            if !key.starts_with(prefix) {
                break;
            }
            let rest = &key[prefix.len()..];
            match delimiter.and_then(|d| rest.find(d).map(|i| (d, i))) {
                Some((d, i)) => {
                    let folder = format!("{prefix}{}", &rest[..i + d.len()]);
                    if entries.last().map(|(k, _)| k) != Some(&folder) {
                        entries.push((folder, None));
                    }
                }
                None => entries.push((
                    key.clone(),
                    Some(ObjectInfo {
                        key: key.clone(),
                        size: data.len() as u64,
                        last_modified: Some(*modified),
                    }),
                )),
            }
        }
        // Resume after the token, and hand out a new one only when entries are left.
        let start = match token {
            Some(t) => entries
                .iter()
                .position(|(k, _)| k.as_str() > t)
                .unwrap_or(entries.len()),
            None => 0,
        };
        let page: Vec<_> = entries.iter().skip(start).take(self.page_size).collect();
        let next_token = (start + page.len() < entries.len())
            .then(|| page.last().map(|(k, _)| k.clone()))
            .flatten();

        // Sort the page back into folders and objects.
        let mut listing = Listing {
            next_token,
            ..Listing::default()
        };
        for (key, info) in page {
            match info {
                Some(info) => listing.objects.push(info.clone()),
                None => listing.prefixes.push(key.clone()),
            }
        }
        Ok(listing)
    }

    /// The range cut from the whole object, empty when it starts past the end, as `S3Client`'s is.
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        let data = self.get(bucket, key)?;
        let len = data.len() as u64;
        if start >= len {
            return Ok(Vec::new());
        }
        let end = end.min(len - 1);
        Ok(data[start as usize..=end as usize].to_vec())
    }

    /// A copy of the object's bytes, or the 404 S3 would give. There's no size cap here.
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        let buckets = self.buckets.lock().unwrap();
        let objects = buckets.get(bucket).ok_or_else(|| Self::no_bucket(bucket))?;
        objects
            .get(key)
            .map(|(d, _)| d.clone())
            .ok_or_else(|| Self::no_key(key))
    }

    /// Remove the object. Only a missing bucket is an error.
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        let mut buckets = self.buckets.lock().unwrap();
        let objects = buckets
            .get_mut(bucket)
            .ok_or_else(|| Self::no_bucket(bucket))?;
        // S3 DeleteObject succeeds on a missing key; so does this.
        objects.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod memory_store_tests {
    use super::*;

    /// With two a page, the folders fill the first page and the loose keys the second, which is
    /// the last.
    #[test]
    fn folders_collapse_and_pages_continue() {
        let s = MemoryStore::new().with_page_size(2);
        for k in ["a/1", "a/2", "b/1", "c", "d"] {
            s.put("bk", k, b"x");
        }
        let p1 = s.list("bk", "", Some("/"), None).unwrap();
        assert_eq!(p1.prefixes, vec!["a/", "b/"]);
        let p2 = s
            .list("bk", "", Some("/"), p1.next_token.as_deref())
            .unwrap();
        assert_eq!(
            p2.objects
                .iter()
                .map(|o| o.key.as_str())
                .collect::<Vec<_>>(),
            ["c", "d"]
        );
        assert_eq!(p2.next_token, None);
    }

    /// A range past the end is cut to the object, and a delete really removes it.
    #[test]
    fn range_clamps_and_delete_removes() {
        let s = MemoryStore::new();
        s.put("bk", "k", b"hello");
        assert_eq!(s.get_range("bk", "k", 1, 99).unwrap(), b"ello");
        s.delete("bk", "k").unwrap();
        assert!(!s.contains("bk", "k"));
    }
}
