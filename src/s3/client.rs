//! `S3Client`: the `Store` over real S3 (or MinIO), with per-bucket region redirects.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use time::OffsetDateTime;

use super::sigv4::{self, EMPTY_SHA256, uri_encode};
use super::transport::{HttpRequest, HttpResponse, Transport, UreqTransport};
use super::{Bucket, Credentials, Listing, S3Error, Store, xml};

/// The real client. Handles buckets in other regions by following S3's region hint.
pub struct S3Client {
    creds: Credentials,
    region: String,
    endpoint: Option<String>,
    path_style: bool,
    max_keys: Option<u32>,
    transport: Arc<dyn Transport>,
    /// Regions S3 told us about, per bucket, so only the first call pays for the redirect.
    bucket_regions: Mutex<HashMap<String, String>>,
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

/// One S3 call, before it's addressed and signed for a particular region.
struct Call<'a> {
    method: &'static str,
    bucket: Option<&'a str>,
    key: Option<&'a str>,
    query: Vec<(&'static str, String)>,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl<'a> Call<'a> {
    fn new(method: &'static str, bucket: Option<&'a str>, key: Option<&'a str>) -> Self {
        Self {
            method,
            bucket,
            key,
            query: Vec::new(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// Bucket names that can go in a hostname. Anything else (upper case, underscores, legacy
/// names) has to use the path.
fn dns_compatible(bucket: &str) -> bool {
    (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && !bucket.starts_with(['-', '.'])
        && !bucket.ends_with(['-', '.'])
        && !bucket.contains("..")
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
            bucket_regions: Mutex::new(HashMap::new()),
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
    pub fn bucket_region(&self, bucket: &str) -> Option<String> {
        self.bucket_regions.lock().unwrap().get(bucket).cloned()
    }

    /// Create a bucket. reses never does this itself; the integration tests need it.
    pub fn create_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        let mut call = Call::new("PUT", Some(bucket), None);
        if self.region != "us-east-1" {
            call.body = format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                self.region
            )
            .into_bytes();
        }
        self.guard(self.call(&call).and_then(ok_status).map(drop))
    }

    /// Upload an object. reses never does this itself; the integration tests need it.
    pub fn put_object(&self, bucket: &str, key: &str, data: &[u8]) -> Result<(), S3Error> {
        let mut call = Call::new("PUT", Some(bucket), Some(key));
        call.body = data.to_vec();
        self.guard(self.call(&call).and_then(ok_status).map(drop))
    }

    /// Scheme, host and path for a call in `region`.
    fn address(&self, call: &Call<'_>, region: &str) -> (String, String, String) {
        let (scheme, authority, base) = match &self.endpoint {
            Some(url) => {
                let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
                let (authority, base) = match rest.find('/') {
                    Some(i) => (&rest[..i], &rest[i..]),
                    None => (rest, ""),
                };
                (scheme.to_string(), authority.to_string(), base.to_string())
            }
            None => (
                "https".to_string(),
                format!("s3.{region}.amazonaws.com"),
                String::new(),
            ),
        };
        let key_path = call.key.map(|k| format!("/{}", uri_encode(k, true)));
        match call.bucket {
            None => (scheme, authority, format!("{base}/")),
            Some(bucket) => {
                // A dotted bucket breaks the *.host wildcard certificate over TLS.
                let virtual_host = !self.path_style
                    && dns_compatible(bucket)
                    && !(scheme == "https" && bucket.contains('.'));
                if virtual_host {
                    let path = key_path.unwrap_or_else(|| "/".to_string());
                    (
                        scheme,
                        format!("{bucket}.{authority}"),
                        format!("{base}{path}"),
                    )
                } else {
                    let path = format!(
                        "{base}/{}{}",
                        uri_encode(bucket, true),
                        key_path.unwrap_or_default()
                    );
                    (scheme, authority, path)
                }
            }
        }
    }

    /// Address, sign and send a call in one region.
    fn send_in(&self, call: &Call<'_>, region: &str) -> Result<HttpResponse, S3Error> {
        let (scheme, host, path) = self.address(call, region);
        let query: Vec<(&str, &str)> = call.query.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let headers: Vec<(&str, &str)> =
            call.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let payload = if call.body.is_empty() {
            EMPTY_SHA256.to_string()
        } else {
            sigv4::sha256_hex(&call.body)
        };
        let signed = sigv4::sign(
            &sigv4::Request {
                method: call.method,
                host: &host,
                path: &path,
                query: &query,
                headers: &headers,
                payload_sha256: &payload,
            },
            &self.creds,
            region,
            "s3",
            OffsetDateTime::now_utc(),
        );
        let qs = sigv4::canonical_query(&query);
        let url = if qs.is_empty() {
            format!("{scheme}://{host}{path}")
        } else {
            format!("{scheme}://{host}{path}?{qs}")
        };
        let mut all_headers: Vec<(String, String)> = call
            .headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        all_headers.extend(signed.headers);
        self.transport
            .send(&HttpRequest {
                method: call.method.to_string(),
                url,
                headers: all_headers,
                body: call.body.clone(),
                body_limit: Default::default(),
            })
            .map_err(S3Error::Transport)
    }

    /// Send a call, following one region redirect for its bucket.
    fn call(&self, call: &Call<'_>) -> Result<HttpResponse, S3Error> {
        let Some(bucket) = call.bucket else {
            return self.send_in(call, &self.region);
        };
        let region = self
            .bucket_region(bucket)
            .unwrap_or_else(|| self.region.clone());
        let resp = self.send_in(call, &region)?;
        match redirect_region(&resp) {
            Some(hint) if hint != region => {
                self.bucket_regions
                    .lock()
                    .unwrap()
                    .insert(bucket.to_string(), hint.clone());
                self.send_in(call, &hint)
            }
            _ => Ok(resp),
        }
    }

    /// Take every credential out of an error before it leaves the client.
    fn scrub(&self, text: String) -> String {
        let mut secrets = vec![self.creds.secret_access_key.clone()];
        if let Some(token) = &self.creds.session_token {
            secrets.push(token.clone());
        }
        let mut text = text;
        for secret in secrets.iter().filter(|s| !s.is_empty()) {
            for form in [secret.clone(), uri_encode(secret, false)] {
                text = text.replace(&form, "<redacted>");
            }
        }
        text
    }

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
}

/// The region S3 is pointing us at, if this response is a wrong-region answer.
fn redirect_region(resp: &HttpResponse) -> Option<String> {
    if resp.status != 301 && resp.status != 400 {
        return None;
    }
    let doc = xml::parse_error(&resp.body).ok();
    if resp.status == 400 {
        let code = doc.as_ref().map(|d| d.code.as_str());
        if !matches!(
            code,
            Some("AuthorizationHeaderMalformed") | Some("PermanentRedirect")
        ) {
            return None;
        }
    }
    resp.header("x-amz-bucket-region")
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| doc.and_then(|d| d.region))
}

/// Turn a non-2xx answer into `S3Error::Service`, from its error document when it has one.
fn ok_status(resp: HttpResponse) -> Result<HttpResponse, S3Error> {
    if (200..300).contains(&resp.status) {
        return Ok(resp);
    }
    Err(service_error(&resp))
}

fn service_error(resp: &HttpResponse) -> S3Error {
    match xml::parse_error(&resp.body) {
        Ok(doc) => S3Error::Service {
            status: resp.status,
            code: doc.code,
            message: doc.message,
        },
        Err(_) => S3Error::Service {
            status: resp.status,
            code: format!("Http{}", resp.status),
            message: "the response had no S3 error document".into(),
        },
    }
}

impl Store for S3Client {
    fn bucket_region(&self, bucket: &str) -> Option<String> {
        S3Client::bucket_region(self, bucket)
    }

    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        let call = Call::new("GET", None, None);
        self.guard(
            self.call(&call)
                .and_then(ok_status)
                .and_then(|r| xml::parse_buckets(&r.body)),
        )
    }

    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error> {
        let mut call = Call::new("GET", Some(bucket), None);
        call.query.push(("list-type", "2".into()));
        // Keys come back percent-encoded, so ones XML can't carry still survive.
        call.query.push(("encoding-type", "url".into()));
        call.query.push(("prefix", prefix.into()));
        if let Some(d) = delimiter {
            call.query.push(("delimiter", d.into()));
        }
        if let Some(t) = token {
            call.query.push(("continuation-token", t.into()));
        }
        if let Some(n) = self.max_keys {
            call.query.push(("max-keys", n.to_string()));
        }
        self.guard(
            self.call(&call)
                .and_then(ok_status)
                .and_then(|r| xml::parse_listing(&r.body)),
        )
    }

    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        if end < start {
            return Ok(Vec::new());
        }
        let mut call = Call::new("GET", Some(bucket), Some(key));
        call.headers.push(("range", format!("bytes={start}-{end}")));
        let result = self.call(&call).and_then(|resp| match resp.status {
            // Past the end (or an empty object): nothing to read, not a failure.
            416 => Ok(Vec::new()),
            206 => {
                let mut body = resp.body;
                body.truncate(usize::try_from(end - start + 1).unwrap_or(usize::MAX));
                Ok(body)
            }
            // The server ignored Range and sent the whole object; cut it here.
            200 => {
                let body = resp.body;
                let len = body.len() as u64;
                if start >= len {
                    return Ok(Vec::new());
                }
                let last = end.min(len - 1);
                Ok(body[start as usize..=last as usize].to_vec())
            }
            _ => Err(service_error(&resp)),
        });
        self.guard(result)
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        let call = Call::new("GET", Some(bucket), Some(key));
        self.guard(self.call(&call).and_then(ok_status).map(|r| r.body))
    }

    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        let call = Call::new("DELETE", Some(bucket), Some(key));
        self.guard(self.call(&call).and_then(ok_status).map(drop))
    }
}
