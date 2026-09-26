//! A small S3 client: SigV4 signing over ureq, just the calls reses needs.
//!
//! Everything above this module talks to the `Store` trait, so the TUI can run against
//! `MemoryStore` in tests.

mod client;
pub mod sigv4;
pub mod transport;
pub mod xml;

pub use client::S3Client;
pub use transport::{HttpRequest, HttpResponse, Transport, UreqTransport};

use std::collections::BTreeMap;
use std::sync::Mutex;

use time::OffsetDateTime;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub name: String,
    pub created: Option<OffsetDateTime>,
}

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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum S3Error {
    /// S3 answered with an error document.
    #[error("{code} ({status}): {message}")]
    Service {
        status: u16,
        code: String,
        message: String,
    },
    #[error("network error: {0}")]
    Transport(String),
    #[error("unexpected response: {0}")]
    Parse(String),
}

impl S3Error {
    pub fn is_not_found(&self) -> bool {
        matches!(self, S3Error::Service { status: 404, .. })
    }
}

pub trait Store: Send + Sync {
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
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error>;
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error>;
}

/// In-memory store for tests. Page size is small on purpose so paging gets exercised.
type Objects = BTreeMap<String, (Vec<u8>, OffsetDateTime)>;

pub struct MemoryStore {
    buckets: Mutex<BTreeMap<String, Objects>>,
    page_size: usize,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(BTreeMap::new()),
            page_size: 3,
        }
    }

    pub fn with_page_size(mut self, n: usize) -> Self {
        self.page_size = n.max(1);
        self
    }

    pub fn put(&self, bucket: &str, key: &str, data: &[u8]) {
        let mut b = self.buckets.lock().unwrap();
        b.entry(bucket.to_string())
            .or_default()
            .insert(key.to_string(), (data.to_vec(), OffsetDateTime::UNIX_EPOCH));
    }

    pub fn create_bucket(&self, bucket: &str) {
        self.buckets
            .lock()
            .unwrap()
            .entry(bucket.to_string())
            .or_default();
    }

    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        self.buckets
            .lock()
            .unwrap()
            .get(bucket)
            .is_some_and(|b| b.contains_key(key))
    }

    fn no_bucket(bucket: &str) -> S3Error {
        S3Error::Service {
            status: 404,
            code: "NoSuchBucket".into(),
            message: format!("bucket {bucket} does not exist"),
        }
    }

    fn no_key(key: &str) -> S3Error {
        S3Error::Service {
            status: 404,
            code: "NoSuchKey".into(),
            message: format!("key {key} does not exist"),
        }
    }
}

impl Store for MemoryStore {
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

    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        let data = self.get(bucket, key)?;
        let len = data.len() as u64;
        if start >= len {
            return Ok(Vec::new());
        }
        let end = end.min(len - 1);
        Ok(data[start as usize..=end as usize].to_vec())
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        let buckets = self.buckets.lock().unwrap();
        let objects = buckets.get(bucket).ok_or_else(|| Self::no_bucket(bucket))?;
        objects
            .get(key)
            .map(|(d, _)| d.clone())
            .ok_or_else(|| Self::no_key(key))
    }

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

    #[test]
    fn range_clamps_and_delete_removes() {
        let s = MemoryStore::new();
        s.put("bk", "k", b"hello");
        assert_eq!(s.get_range("bk", "k", 1, 99).unwrap(), b"ello");
        s.delete("bk", "k").unwrap();
        assert!(!s.contains("bk", "k"));
    }
}
