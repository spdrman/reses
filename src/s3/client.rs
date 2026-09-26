//! `S3Client`: the `Store` over aws-sdk-s3. Not built yet; the tests describe it.

use std::path::Path;

use super::{Bucket, Credentials, Listing, S3Error, Store};

/// The real client.
pub struct S3Client {
    region: String,
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client").finish_non_exhaustive()
    }
}

/// The error every stub answers with.
fn stub() -> S3Error {
    S3Error::Transport("S3 client is not built yet".into())
}

impl S3Client {
    /// A client with fixed keys.
    pub fn new(creds: Credentials, region: &str) -> Self {
        let _ = creds;
        Self {
            region: region.to_string(),
        }
    }
    /// A client for a named profile in the default AWS files.
    pub fn from_profile(name: &str, region_hint: Option<&str>) -> Self {
        let _ = name;
        Self {
            region: region_hint.unwrap_or("us-east-1").to_string(),
        }
    }
    /// A client for a named profile in these files.
    pub fn from_profile_files(
        name: &str,
        region_hint: Option<&str>,
        config_file: &Path,
        credentials_file: &Path,
    ) -> Self {
        let _ = (config_file, credentials_file);
        Self::from_profile(name, region_hint)
    }
    /// Point at another endpoint.
    pub fn with_endpoint(self, url: &str, path_style: bool) -> Self {
        let _ = (url, path_style);
        self
    }
    /// Swap the SDK's HTTP client.
    pub fn with_http_client(self, client: impl aws_sdk_s3::config::HttpClient + 'static) -> Self {
        let _ = client;
        self
    }
    /// Keys per listing page.
    pub fn with_max_keys(self, n: u32) -> Self {
        let _ = n;
        self
    }
    /// The default region.
    pub fn region(&self) -> &str {
        &self.region
    }
    /// The region learned for a bucket.
    pub fn bucket_region(&self, bucket: &str) -> Option<String> {
        let _ = bucket;
        None
    }
    /// Create a bucket.
    pub fn create_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        let _ = bucket;
        Err(stub())
    }
    /// Upload an object.
    pub fn put_object(&self, bucket: &str, key: &str, data: &[u8]) -> Result<(), S3Error> {
        let _ = (bucket, key, data);
        Err(stub())
    }
}

impl Store for S3Client {
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        Err(stub())
    }
    fn list(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Listing, S3Error> {
        Err(stub())
    }
    fn get_range(&self, _: &str, _: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
        Err(stub())
    }
    fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, S3Error> {
        Err(stub())
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), S3Error> {
        Err(stub())
    }
}
