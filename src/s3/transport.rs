//! The HTTP layer under `S3Client`, behind a trait so tests can script responses.

use std::io::Read;
use std::time::Duration;

/// SES refuses messages over 40 MB, so a whole-object read stops a little past that.
pub const MAX_GET_BYTES: u64 = 41 * 1024 * 1024;
/// What a ranged read takes on top of the bytes it asked for.
pub const RANGE_SLACK: u64 = 4096;
/// A ListObjectsV2 or ListBuckets page: 1000 keys of up to 1 KiB each, with room to spare.
pub const LIST_BODY_LIMIT: u64 = 16 * 1024 * 1024;
/// An error document, or the answer to a PUT or DELETE.
pub const ERROR_BODY_LIMIT: u64 = 1024 * 1024;

/// How much of a response body the transport reads before it stops. A 206 gets `partial`,
/// any other 2xx gets `ok`, and everything else gets `ERROR_BODY_LIMIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimit {
    pub ok: u64,
    pub partial: u64,
}

impl Default for BodyLimit {
    fn default() -> Self {
        Self {
            ok: ERROR_BODY_LIMIT,
            partial: ERROR_BODY_LIMIT,
        }
    }
}

impl BodyLimit {
    pub fn for_status(&self, status: u16) -> u64 {
        match status {
            206 => self.partial,
            200..=299 => self.ok,
            _ => ERROR_BODY_LIMIT,
        }
    }
}

/// One HTTP request. Its Debug output leaves out header values that carry credentials.
#[derive(Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub body_limit: BodyLimit,
}

impl HttpRequest {
    /// First header with this name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Headers whose values are credentials or derived from them.
const SENSITIVE: &[&str] = &["authorization", "x-amz-security-token"];

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let hidden = SENSITIVE.iter().any(|s| k.eq_ignore_ascii_case(s));
                (k.as_str(), if hidden { "<redacted>" } else { v.as_str() })
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .field("body_limit", &self.body_limit)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The body ran past the request's limit, so `body` holds only its first part.
    pub truncated: bool,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends a request and hands back whatever status came back. `Err` means no HTTP
/// response at all (DNS, connect, TLS, timeout).
pub trait Transport: Send + Sync {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String>;
}

/// The real transport: ureq with rustls and bundled webpki roots.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqTransport {
    pub fn new() -> Self {
        Self::with_timeouts(
            Duration::from_secs(15),
            Duration::from_secs(60),
            Duration::from_secs(300),
        )
    }

    /// `connect` and `response` cover getting as far as the status line. `body` is the whole
    /// budget for reading the body, so a server that stalls halfway can't hang a worker.
    pub fn with_timeouts(connect: Duration, response: Duration, body: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            // Every status comes back as a response; the client reads S3's error documents.
            .http_status_as_error(false)
            // A 301 from S3 is a region hint, not something to follow.
            .max_redirects(0)
            .max_redirects_will_error(false)
            // Object bytes must arrive exactly as stored, so never ask for compression.
            .accept_encoding(ureq::config::AutoHeaderValue::None)
            .user_agent(concat!("reses/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(connect))
            .timeout_recv_response(Some(response))
            .timeout_recv_body(Some(body))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Transport for UreqTransport {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        let mut builder = ureq::http::Request::builder()
            .method(req.method.as_str())
            .uri(req.url.as_str());
        for (k, v) in &req.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        // GET and DELETE go without a body at all; PUT always says how long its body is.
        let result = if req.body.is_empty() && req.method != "PUT" {
            let request = builder.body(()).map_err(|e| format!("bad request: {e}"))?;
            self.agent.run(request)
        } else {
            let request = builder
                .body(req.body.as_slice())
                .map_err(|e| format!("bad request: {e}"))?;
            self.agent.run(request)
        };
        let mut resp = result.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        // Read one byte past the limit to tell "exactly the limit" from "more than that", and
        // stop there rather than draining the rest.
        let limit = req.body_limit.for_status(status);
        let mut body = Vec::new();
        resp.body_mut()
            .with_config()
            .limit(u64::MAX)
            .reader()
            .take(limit.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|e| format!("reading the response: {e}"))?;
        let truncated = body.len() as u64 > limit;
        if truncated {
            body.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
            truncated,
        })
    }
}
