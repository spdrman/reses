//! `S3Client`: the `Store` over real S3 (or MinIO), with per-bucket region redirects.

use std::sync::Arc;

use super::transport::{Transport, UreqTransport};
use super::{Bucket, Credentials, Listing, S3Error, Store};

/// The real client. Handles buckets in other regions by following S3's region hint.
pub struct S3Client {
    creds: Credentials,
    region: String,
    endpoint: Option<String>,
    path_style: bool,
    max_keys: Option<u32>,
    transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client")
            .field("creds", &self.creds)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("path_style", &self.path_style)
            .finish_non_exhaustive()
    }
}

impl S3Client {
    pub fn new(creds: Credentials, region: &str) -> Self {
        Self {
            creds,
            region: region.to_string(),
            endpoint: None,
            path_style: false,
            max_keys: None,
            transport: Arc::new(UreqTransport::new()),
        }
    }

    /// Point at a non-AWS endpoint (MinIO in tests). `path_style` puts the bucket in the path.
    pub fn with_endpoint(mut self, url: &str, path_style: bool) -> Self {
        self.endpoint = Some(url.trim_end_matches('/').to_string());
        self.path_style = path_style;
        self
    }

    /// Swap the HTTP layer, so tests can script S3's answers.
    pub fn with_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = transport;
        self
    }

    /// Ask for at most `n` keys per `list` page (S3's default is 1000).
    pub fn with_max_keys(mut self, n: u32) -> Self {
        self.max_keys = Some(n);
        self
    }

    /// The region this client has learned for `bucket`, if S3 redirected it.
    pub fn bucket_region(&self, _bucket: &str) -> Option<String> {
        None
    }

    /// Create a bucket. reses never does this itself; the integration tests need it.
    pub fn create_bucket(&self, _bucket: &str) -> Result<(), S3Error> {
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }

    /// Upload an object. reses never does this itself; the integration tests need it.
    pub fn put_object(&self, _bucket: &str, _key: &str, _data: &[u8]) -> Result<(), S3Error> {
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }
}

impl Store for S3Client {
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        let _ = (&self.creds, &self.transport, self.max_keys);
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }
    fn list(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Listing, S3Error> {
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }
    fn get_range(&self, _: &str, _: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }
    fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, S3Error> {
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), S3Error> {
        Err(S3Error::Transport("S3 client is not built yet".into()))
    }
}
