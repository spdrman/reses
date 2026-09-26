//! `S3Client`: the synchronous `Store` the screens use, over aws-sdk-s3.
//!
//! The SDK does the real work: signing, retries, XML, TLS, and (through aws-config) resolving a
//! profile's credentials, whether they're static keys, a session token, SSO, assume-role or
//! credential_process. The screens call `Store` from worker threads and never see async code, so
//! I run every call on one small tokio runtime that reses owns and block on it here.
//!
//! On top of the SDK I keep the few promises the old client made, because the screens rely on
//! them: a range past the end reads as empty rather than failing, a whole get stops at
//! `MAX_GET_BYTES` with a typed `TooLarge` error, an empty key never reaches S3, listing keys
//! come up raw (decoded from `encoding-type=url`, never escaped for display, which the screens
//! do), a bucket in another region works after one redirect, and no secret I know of ever
//! appears in an error or in Debug output.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_config::meta::region::ProvideRegion;
use aws_config::profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::{Builder as ConfigBuilder, Credentials as SdkCredentials, Region};
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration, EncodingType};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use time::OffsetDateTime;

use super::{
    Bucket, Credentials, Listing, MAX_GET_BYTES, ObjectInfo, RANGE_SLACK, S3Error, Store,
    valid_region,
};

/// How long reading one object's body may take in total, so a server that stalls halfway can't
/// park a worker forever. The SDK's own stalled-stream check usually fires well before this.
const BODY_TIMEOUT: Duration = Duration::from_secs(300);
/// What URI encoding leaves alone (RFC 3986's unreserved characters). Everything else in a
/// secret gets a `%XX` escape when a server echoes it back encoded.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
/// The region I fall back to when neither the caller nor the profile names a usable one.
const FALLBACK_REGION: &str = "us-east-1";

/// The runtime every S3 call runs on. I build it once, on first use, with two worker threads:
/// the screens' job pool already provides the concurrency, so this only has to drive the I/O.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("reses-s3")
            .enable_all()
            .build()
            .expect("reses can't start its S3 runtime")
    })
}

/// The real client. It holds the SDK configuration minus the region, and builds one SDK client
/// per region on demand, so a bucket that lives elsewhere just gets that region's client.
pub struct S3Client {
    /// Everything the SDK needs except the region: credentials, endpoint, HTTP client, timeouts.
    base: ConfigBuilder,
    /// The region calls go to until S3 says a bucket lives somewhere else.
    region: String,
    /// The profile the credentials come from, for Debug output. `None` for fixed keys.
    profile: Option<String>,
    /// A custom endpoint (MinIO), for Debug output.
    endpoint: Option<String>,
    /// Secrets I know the text of, so I can strip them from any error message.
    secrets: Vec<String>,
    /// Keys per listing page, when set; S3 defaults to 1000.
    max_keys: Option<i32>,
    /// One SDK client per region, built the first time that region is needed.
    clients: Mutex<HashMap<String, Client>>,
    /// Regions S3 told me about, per bucket, so only the first call pays for the redirect.
    bucket_regions: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for S3Client {
    /// I list only what identifies the client. Credentials never appear, not even the SDK's
    /// own redacted form.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client")
            .field("profile", &self.profile)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Settings every client shares: the SDK's current behaviour defaults, and timeouts that give
/// up on a dead connection instead of waiting on it.
fn with_defaults(builder: ConfigBuilder) -> ConfigBuilder {
    builder
        .behavior_version(BehaviorVersion::latest())
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(15))
                .operation_attempt_timeout(Duration::from_secs(60))
                .build(),
        )
}

/// An empty key would address the bucket itself, so I refuse it before any request goes out.
fn require_key(key: &str) -> Result<(), S3Error> {
    if key.is_empty() {
        Err(S3Error::EmptyKey)
    } else {
        Ok(())
    }
}

/// Undo `encoding-type=url` on a key or prefix. S3 applies it form-style, so `+` is a space and
/// `%2B` a plus. When the response didn't say it encoded anything, I leave the text alone.
fn decode_key(text: &str, url_encoded: bool) -> Result<String, S3Error> {
    if !url_encoded {
        return Ok(text.to_string());
    }
    percent_decode_str(&text.replace('+', " "))
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| S3Error::Parse(format!("a key isn't UTF-8 once decoded: {text}")))
}

/// `%2F` written as `%2f`: the hex digits of every escape in lower case, the rest untouched.
fn lowercase_escapes(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    let mut hex_left = 0;
    for c in encoded.chars() {
        if hex_left > 0 {
            out.push(c.to_ascii_lowercase());
            hex_left -= 1;
        } else {
            if c == '%' {
                hex_left = 2;
            }
            out.push(c);
        }
    }
    out
}

/// An SDK timestamp as the `time` type the rest of reses uses.
fn to_offset(dt: &aws_sdk_s3::primitives::DateTime) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(dt.as_nanos()).ok()
}

/// Turn an SDK error into reses's error. An answer from S3 keeps its status, code and message;
/// a response the SDK couldn't read is a `Parse`; anything short of a response (DNS, connect,
/// TLS, timeouts, credentials that couldn't be resolved) is a `Transport` with the SDK's full
/// explanation.
fn map_sdk_error<E>(e: SdkError<E>) -> S3Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match &e {
        SdkError::ServiceError(se) => {
            let status = se.raw().status().as_u16();
            S3Error::Service {
                status,
                code: se
                    .err()
                    .code()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Http{status}")),
                message: se.err().message().unwrap_or_default().to_string(),
            }
        }
        SdkError::ResponseError(_) => S3Error::Parse(DisplayErrorContext(&e).to_string()),
        _ => S3Error::Transport(DisplayErrorContext(&e).to_string()),
    }
}

/// When an error is S3 saying "this bucket is in another region", I hand back the region its
/// `x-amz-bucket-region` header names (if it's a usable region name), wrapped in `Some`. A 301,
/// or a 400 `AuthorizationHeaderMalformed` or `PermanentRedirect`, counts; anything else is
/// `None`, not a redirect.
fn redirect_hint<E: ProvideErrorMetadata>(e: &SdkError<E>) -> Option<Option<String>> {
    let SdkError::ServiceError(se) = e else {
        return None;
    };
    let status = se.raw().status().as_u16();
    let code = se.err().code();
    let redirect = status == 301
        || (status == 400
            && matches!(
                code,
                Some("AuthorizationHeaderMalformed") | Some("PermanentRedirect")
            ));
    if !redirect {
        return None;
    }
    Some(region_header(se.raw().headers().get("x-amz-bucket-region")))
}

/// A region header's value, if it's a region name. Anything else is ignored, since the region
/// ends up in a hostname and in the signing scope.
fn region_header(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|r| valid_region(r))
        .map(str::to_string)
}

/// Read an object body, stopping once it has more than `limit` bytes. The flag says it stopped
/// early, and the bytes are cut back to `limit`. I read one byte past the limit so a body of
/// exactly the limit isn't taken for a bigger one.
async fn read_body(mut body: ByteStream, limit: u64) -> Result<(Vec<u8>, bool), S3Error> {
    let read = async {
        let mut out: Vec<u8> = Vec::new();
        let want = usize::try_from(limit.saturating_add(1)).unwrap_or(usize::MAX);
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|e| S3Error::Transport(format!("reading the object: {e}")))?
        {
            // Take only what fits, and stop as soon as the body has run past the limit.
            let room = want - out.len();
            out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if out.len() == want {
                out.truncate(want - 1);
                return Ok((out, true));
            }
        }
        Ok((out, false))
    };
    tokio::time::timeout(BODY_TIMEOUT, read)
        .await
        .map_err(|_| S3Error::Transport("timed out reading the object".into()))?
}

/// The profile files to read: these two when given, otherwise the SDK's defaults
/// (`~/.aws/config` and `~/.aws/credentials`, or wherever the AWS_* variables point).
fn profile_files(files: Option<(&Path, &Path)>) -> Option<EnvConfigFiles> {
    let (config, credentials) = files?;
    Some(
        EnvConfigFiles::builder()
            .with_file(EnvConfigFileKind::Config, config)
            .with_file(EnvConfigFileKind::Credentials, credentials)
            .build(),
    )
}

impl S3Client {
    /// A client with fixed keys, used for MinIO and the tests. A region that isn't a region name
    /// becomes us-east-1, and a redirect then finds the bucket's real one.
    pub fn new(creds: Credentials, region: &str) -> Self {
        let region = if valid_region(region) {
            region
        } else {
            FALLBACK_REGION
        };
        // I remember the secret and token so they can be stripped from any error text.
        let mut secrets = vec![creds.secret_access_key.clone()];
        secrets.extend(creds.session_token.clone());
        let provider = SdkCredentials::new(
            creds.access_key_id,
            creds.secret_access_key,
            creds.session_token,
            None,
            "reses",
        );
        Self::from_parts(
            with_defaults(ConfigBuilder::new()).credentials_provider(provider),
            region.to_string(),
            None,
            secrets,
        )
    }

    /// A client for a named profile in the default AWS files. aws-config resolves the
    /// credentials itself (static keys, session tokens, SSO, assume-role, credential_process),
    /// lazily, on the first request. The region is `region_hint` when it's a region name, else
    /// the profile's `region`, else us-east-1.
    pub fn from_profile(name: &str, region_hint: Option<&str>) -> Self {
        Self::profile_client(name, region_hint, None)
    }

    /// The same as `from_profile`, reading these two files instead of the default ones.
    pub fn from_profile_files(
        name: &str,
        region_hint: Option<&str>,
        config_file: &Path,
        credentials_file: &Path,
    ) -> Self {
        Self::profile_client(name, region_hint, Some((config_file, credentials_file)))
    }

    /// Build a profile client. I set the region and the credentials provider explicitly, so
    /// aws-config never falls back to the instance metadata service (a slow timeout on a laptop)
    /// and never swaps in AWS_ACCESS_KEY_ID from the environment for the profile the user picked.
    fn profile_client(
        name: &str,
        region_hint: Option<&str>,
        files: Option<(&Path, &Path)>,
    ) -> Self {
        let files = profile_files(files);
        let config = runtime().block_on(async {
            // The region: a usable hint, else the profile's own, else the fallback.
            let mut region_provider = ProfileFileRegionProvider::builder().profile_name(name);
            if let Some(f) = &files {
                region_provider = region_provider.profile_files(f.clone());
            }
            let from_profile = region_provider.build().region().await;
            let region = region_hint
                .filter(|r| valid_region(r))
                .map(str::to_string)
                .or_else(|| {
                    from_profile
                        .map(|r| r.to_string())
                        .filter(|r| valid_region(r))
                })
                .unwrap_or_else(|| FALLBACK_REGION.to_string());

            // The credentials: exactly this profile's, from the chosen files.
            let mut creds = ProfileFileCredentialsProvider::builder().profile_name(name);
            if let Some(f) = &files {
                creds = creds.profile_files(f.clone());
            }

            // The rest of the shared settings (endpoint_url, retries and so on) still come
            // from the same profile through aws-config's loader.
            let mut loader = aws_config::defaults(BehaviorVersion::latest())
                .profile_name(name)
                .region(Region::new(region.clone()))
                .credentials_provider(creds.build());
            if let Some(f) = &files {
                loader = loader.profile_files(f.clone());
            }
            (loader.load().await, region)
        });
        let (sdk_config, region) = config;
        Self::from_parts(
            with_defaults(ConfigBuilder::from(&sdk_config)),
            region,
            Some(name.to_string()),
            Vec::new(),
        )
    }

    /// The fields every constructor ends up with.
    fn from_parts(
        base: ConfigBuilder,
        region: String,
        profile: Option<String>,
        secrets: Vec<String>,
    ) -> Self {
        Self {
            base,
            region,
            profile,
            endpoint: None,
            secrets,
            max_keys: None,
            clients: Mutex::new(HashMap::new()),
            bucket_regions: Mutex::new(HashMap::new()),
        }
    }

    /// Point at a non-AWS endpoint (MinIO in tests). `path_style` puts the bucket in the path.
    pub fn with_endpoint(mut self, url: &str, path_style: bool) -> Self {
        let url = url.trim_end_matches('/');
        self.base = self.base.endpoint_url(url).force_path_style(path_style);
        self.endpoint = Some(url.to_string());
        self.clients.get_mut().unwrap().clear();
        self
    }

    /// Swap the SDK's HTTP client, so tests can script S3's answers.
    pub fn with_http_client(
        mut self,
        client: impl aws_sdk_s3::config::HttpClient + 'static,
    ) -> Self {
        self.base = self.base.http_client(client);
        self.clients.get_mut().unwrap().clear();
        self
    }

    /// Ask for at most `n` keys per `list` page.
    pub fn with_max_keys(mut self, n: u32) -> Self {
        self.max_keys = Some(i32::try_from(n).unwrap_or(i32::MAX));
        self
    }

    /// The region calls go to by default.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The region this client has learned for `bucket`, if S3 redirected it.
    pub fn bucket_region(&self, bucket: &str) -> Option<String> {
        self.bucket_regions.lock().unwrap().get(bucket).cloned()
    }

    /// Create a bucket. reses never does this itself; the integration tests need it.
    pub fn create_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        // Outside us-east-1, S3 wants the region spelled out in the request body.
        let location = (self.region != FALLBACK_REGION).then(|| {
            CreateBucketConfiguration::builder()
                .location_constraint(BucketLocationConstraint::from(self.region.as_str()))
                .build()
        });
        self.guard(runtime().block_on(self.call(bucket, |c| {
            c.create_bucket()
                .bucket(bucket)
                .set_create_bucket_configuration(location.clone())
                .send()
        })))
        .map(drop)
    }

    /// Upload an object. reses never does this itself; the integration tests need it.
    pub fn put_object(&self, bucket: &str, key: &str, data: &[u8]) -> Result<(), S3Error> {
        require_key(key)?;
        self.guard(runtime().block_on(self.call(bucket, |c| {
            c.put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from(data.to_vec()))
                .send()
        })))
        .map(drop)
    }

    /// The SDK client for `region`, built the first time it's asked for.
    fn client_for(&self, region: &str) -> Client {
        let mut clients = self.clients.lock().unwrap();
        clients
            .entry(region.to_string())
            .or_insert_with(|| {
                Client::from_conf(
                    self.base
                        .clone()
                        .region(Region::new(region.to_string()))
                        .build(),
                )
            })
            .clone()
    }

    /// Run one bucket operation. It goes to the region I know for the bucket (or the default).
    /// If S3 answers that the bucket lives elsewhere, I take the region from the answer, or ask
    /// HeadBucket when the answer doesn't say, remember it, and try once more there.
    async fn call<T, E, F, Fut>(&self, bucket: &str, op: F) -> Result<T, S3Error>
    where
        F: Fn(Client) -> Fut,
        Fut: Future<Output = Result<T, SdkError<E>>>,
        E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    {
        let region = self
            .bucket_region(bucket)
            .unwrap_or_else(|| self.region.clone());
        let err = match op(self.client_for(&region)).await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };

        // Not a redirect: that's the answer.
        let Some(hint) = redirect_hint(&err) else {
            return Err(map_sdk_error(err));
        };

        // A redirect: find where the bucket is, and give up on the original error if I can't
        // or if it points back where I already am.
        let found = match hint {
            Some(r) => Some(r),
            None => self.lookup_region(bucket, &region).await,
        };
        let Some(found) = found.filter(|r| *r != region) else {
            return Err(map_sdk_error(err));
        };
        self.bucket_regions
            .lock()
            .unwrap()
            .insert(bucket.to_string(), found.clone());
        op(self.client_for(&found)).await.map_err(map_sdk_error)
    }

    /// Ask S3 where a bucket lives with HeadBucket. A bucket in another region answers with a
    /// 301 that still carries `x-amz-bucket-region`, so I read the header from either outcome.
    async fn lookup_region(&self, bucket: &str, region: &str) -> Option<String> {
        match self
            .client_for(region)
            .head_bucket()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(out) => region_header(out.bucket_region()),
            Err(SdkError::ServiceError(se)) => {
                region_header(se.raw().headers().get("x-amz-bucket-region"))
            }
            Err(_) => None,
        }
    }

    /// Take every secret I know out of an error before it leaves the client: as written, and
    /// URL-encoded in upper and lower case hex, since a server echoing one back may use either.
    fn scrub(&self, text: String) -> String {
        let mut text = text;
        for secret in self.secrets.iter().filter(|s| !s.is_empty()) {
            let encoded = utf8_percent_encode(secret, UNRESERVED).to_string();
            for form in [secret.clone(), lowercase_escapes(&encoded), encoded] {
                text = text.replace(&form, "<redacted>");
            }
        }
        text
    }

    /// Apply `scrub` to whatever text an error carries.
    fn guard<T>(&self, result: Result<T, S3Error>) -> Result<T, S3Error> {
        result.map_err(|e| match e {
            S3Error::Service {
                status,
                code,
                message,
            } => S3Error::Service {
                status,
                code: self.scrub(code),
                message: self.scrub(message),
            },
            S3Error::Transport(m) => S3Error::Transport(self.scrub(m)),
            S3Error::Parse(m) => S3Error::Parse(self.scrub(m)),
            other @ (S3Error::TooLarge { .. } | S3Error::EmptyKey) => other,
        })
    }

    /// Every bucket the account can see, following ListBuckets pages until there are no more
    /// (or a page hands back a token it already gave).
    async fn all_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        let client = self.client_for(&self.region);
        let mut buckets = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let out = client
                .list_buckets()
                .set_continuation_token(token.clone())
                .send()
                .await
                .map_err(map_sdk_error)?;
            for b in out.buckets() {
                buckets.push(Bucket {
                    name: b.name().unwrap_or_default().to_string(),
                    created: b.creation_date().and_then(to_offset),
                });
            }
            let next = out
                .continuation_token()
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if next.is_none() || next == token {
                return Ok(buckets);
            }
            token = next;
        }
    }
}

impl Store for S3Client {
    /// The region learned for `bucket`, so screens holding only a `dyn Store` can show it.
    fn bucket_region(&self, bucket: &str) -> Option<String> {
        S3Client::bucket_region(self, bucket)
    }

    /// Every bucket, from the default region's endpoint (ListBuckets works from any region).
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        self.guard(runtime().block_on(self.all_buckets()))
    }

    /// One ListObjectsV2 page. I ask for url-encoded keys, so keys XML can't carry still
    /// arrive, and decode them only when the response says it encoded them.
    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error> {
        let result = runtime().block_on(async {
            let out = self
                .call(bucket, |c| {
                    c.list_objects_v2()
                        .bucket(bucket)
                        .prefix(prefix)
                        .set_delimiter(delimiter.map(str::to_string))
                        .set_continuation_token(token.map(str::to_string))
                        .set_max_keys(self.max_keys)
                        .encoding_type(EncodingType::Url)
                        .send()
                })
                .await?;
            let encoded = out.encoding_type() == Some(&EncodingType::Url);

            // Folders, then objects, both decoded the same way.
            let mut listing = Listing::default();
            for p in out.common_prefixes() {
                if let Some(prefix) = p.prefix() {
                    listing.prefixes.push(decode_key(prefix, encoded)?);
                }
            }
            for o in out.contents() {
                let Some(key) = o.key() else { continue };
                listing.objects.push(ObjectInfo {
                    key: decode_key(key, encoded)?,
                    size: o.size().and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
                    last_modified: o.last_modified().and_then(to_offset),
                });
            }

            // A page that says it isn't truncated is the last one, whatever token it carries.
            listing.next_token = match out.is_truncated() {
                Some(false) => None,
                _ => out
                    .next_continuation_token()
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
            };
            Ok(listing)
        });
        self.guard(result)
    }

    /// Bytes `start..=end`, clamped to the object. A 206 is read up to its length plus
    /// `RANGE_SLACK`; a 200 (a server ignoring Range) only up to `end`; a 416 is empty.
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        require_key(key)?;
        if end < start {
            return Ok(Vec::new());
        }
        // How much each kind of answer may send before I stop reading it.
        let wanted = end - start + 1;
        let partial_limit = wanted.saturating_add(RANGE_SLACK);
        let whole_limit = end.saturating_add(1).min(MAX_GET_BYTES);
        let range = format!("bytes={start}-{end}");
        let result = runtime().block_on(async {
            let out = match self
                .call(bucket, |c| {
                    c.get_object()
                        .bucket(bucket)
                        .key(key)
                        .range(range.clone())
                        .send()
                })
                .await
            {
                Ok(out) => out,
                // Past the end, or an empty object: nothing to read, not a failure.
                Err(S3Error::Service { status: 416, .. }) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };

            // A partial answer: keep just the range.
            if out.content_range().is_some() {
                let (mut data, _) = read_body(out.body, partial_limit).await?;
                data.truncate(usize::try_from(wanted).unwrap_or(usize::MAX));
                return Ok(data);
            }

            // The whole object: read up to `end`, then cut the range out.
            let size = out.content_length().and_then(|n| u64::try_from(n).ok());
            let (data, cut_short) = read_body(out.body, whole_limit).await?;
            let len = data.len() as u64;
            if cut_short && end >= len {
                return Err(S3Error::TooLarge {
                    size,
                    limit: whole_limit,
                });
            }
            if start >= len {
                return Ok(Vec::new());
            }
            let last = end.min(len - 1);
            Ok(data[start as usize..=last as usize].to_vec())
        });
        self.guard(result)
    }

    /// A whole object, up to `MAX_GET_BYTES`. A bigger one is `TooLarge`, found from its
    /// Content-Length before reading when the server sends one, or by reading up to the cap.
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        require_key(key)?;
        let result = runtime().block_on(async {
            let out = self
                .call(bucket, |c| c.get_object().bucket(bucket).key(key).send())
                .await?;
            // Refuse before reading when the server already says it's too big.
            let size = out.content_length().and_then(|n| u64::try_from(n).ok());
            if size.is_some_and(|n| n > MAX_GET_BYTES) {
                return Err(S3Error::TooLarge {
                    size,
                    limit: MAX_GET_BYTES,
                });
            }
            // Otherwise read up to the cap, in case the length was missing or wrong.
            let (data, cut_short) = read_body(out.body, MAX_GET_BYTES).await?;
            if cut_short {
                return Err(S3Error::TooLarge {
                    size,
                    limit: MAX_GET_BYTES,
                });
            }
            Ok(data)
        });
        self.guard(result)
    }

    /// Delete one object. S3 answers success for a key that isn't there, and so do I.
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        require_key(key)?;
        self.guard(
            runtime()
                .block_on(self.call(bucket, |c| c.delete_object().bucket(bucket).key(key).send())),
        )
        .map(drop)
    }
}
