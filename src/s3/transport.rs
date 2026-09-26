//! The HTTP layer under `S3Client`, behind a trait so tests can script responses.

use std::time::Duration;

/// One HTTP request. Its Debug output leaves out header values that carry credentials.
#[derive(Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
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
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
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
        let config = ureq::Agent::config_builder()
            // Every status comes back as a response; the client reads S3's error documents.
            .http_status_as_error(false)
            // A 301 from S3 is a region hint, not something to follow.
            .max_redirects(0)
            .max_redirects_will_error(false)
            // Object bytes must arrive exactly as stored, so never ask for compression.
            .accept_encoding(ureq::config::AutoHeaderValue::None)
            .user_agent(concat!("reses/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
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
        let body = resp
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(|e| format!("reading the response: {e}"))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}
