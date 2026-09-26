//! `S3Client` over aws-sdk-s3, driven through a scripted HTTP client so every case runs offline.
//!
//! I swap the SDK's HTTP layer for a closure that records each request and answers from a queue.
//! That lets me check what reses promises on top of the SDK: ranged reads past the end, the size
//! cap on a whole get, empty keys refused, listings that end even when a server sends a stray
//! token, cross-region buckets, credentials resolved from profile files (including
//! credential_process), and no secret in any error or Debug output. MinIO never redirects, so the
//! region cases can only be tested this way.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aws_smithy_http_client::test_util::infallible_client_fn;
use aws_smithy_types::body::SdkBody;
use reses::s3::{Credentials, MAX_GET_BYTES, RANGE_SLACK, S3Client, S3Error, Store};

const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEXAMPLE/session+token==";

/// Reads one of the recorded S3 response bodies under tests/fixtures/s3.
fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/s3/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// What the fake saw of one request.
#[derive(Debug, Clone)]
struct Sent {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
}

impl Sent {
    /// The first header with this name, ignoring case.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The region in the SigV4 credential scope, which is what S3 checks against the bucket.
    fn scope_region(&self) -> String {
        let auth = self.header("authorization").expect("signed");
        let cred = auth.split("Credential=").nth(1).unwrap();
        cred.split('/').nth(2).unwrap().to_string()
    }

    /// The host the request went to.
    fn host(&self) -> String {
        let rest = self.uri.split("://").nth(1).unwrap();
        rest.split('/').next().unwrap().to_string()
    }
}

/// One scripted answer: status, headers, body, and whether the body breaks after its bytes.
type Answer = (u16, Vec<(String, String)>, Vec<u8>, bool);

/// A body that hands over its bytes in one chunk and then fails. A reader that stops once it
/// has enough never sees the failure; one that drains the whole body does.
struct ThenFail {
    data: Option<bytes::Bytes>,
}

impl http_body::Body for ThenFail {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    /// The bytes first, then an error, for as long as anyone keeps asking.
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>> {
        std::task::Poll::Ready(Some(match self.data.take() {
            Some(d) => Ok(http_body::Frame::data(d)),
            None => Err(std::io::Error::other("the body broke")),
        }))
    }
}

/// Records requests and answers them from a queue, in order.
#[derive(Default)]
struct Fake {
    answers: Mutex<VecDeque<Answer>>,
    sent: Mutex<Vec<Sent>>,
}

impl Fake {
    /// A fake that will give these answers, one per request.
    fn new(answers: Vec<Answer>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            sent: Mutex::default(),
        })
    }

    /// Everything sent so far.
    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }

    /// Queue another answer.
    fn push(&self, a: Answer) {
        self.answers.lock().unwrap().push_back(a);
    }

    /// The fake as an SDK HTTP client. A request with no answer queued gets a 599, which the
    /// tests never expect, so an extra request shows up as a failure rather than a hang.
    fn http(self: &Arc<Self>) -> aws_sdk_s3::config::SharedHttpClient {
        let fake = self.clone();
        infallible_client_fn(move |req| {
            fake.sent.lock().unwrap().push(Sent {
                method: req.method().to_string(),
                uri: req.uri().to_string(),
                headers: req
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
            });
            let (status, headers, body, fail) = fake
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or((599, vec![], b"no answer queued".to_vec(), false));
            let mut resp = http::Response::builder().status(status);
            for (k, v) in headers {
                resp = resp.header(k, v);
            }
            let body = if fail {
                SdkBody::from_body_1_x(ThenFail {
                    data: Some(body.into()),
                })
            } else {
                SdkBody::from(body)
            };
            resp.body(body).unwrap()
        })
    }
}

/// A plain answer with no headers beyond Content-Length.
fn ok(status: u16, body: &[u8]) -> Answer {
    with_headers(status, &[], body)
}

/// An answer with headers. Content-Length is added unless the test sets its own.
fn with_headers(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if !h
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        h.push(("content-length".into(), body.len().to_string()));
    }
    (status, h, body.to_vec(), false)
}

/// An answer whose body breaks once its bytes are read, with no Content-Length.
fn then_fail(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> Answer {
    let h = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (status, h, body, true)
}

/// An S3 error document with this code and message.
fn error_doc(code: &str, message: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{message}</Message><RequestId>R</RequestId></Error>"
    )
    .into_bytes()
}

/// The fixed test keys, with or without a session token.
fn creds(token: Option<&str>) -> Credentials {
    Credentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_access_key: SECRET.into(),
        session_token: token.map(str::to_string),
    }
}

/// A client for real AWS addressing in `region`, talking to the fake.
fn aws(region: &str, fake: &Arc<Fake>) -> S3Client {
    S3Client::new(creds(None), region).with_http_client(fake.http())
}

/// A client for a MinIO-style endpoint with path-style addressing, talking to the fake.
fn minio(fake: &Arc<Fake>) -> S3Client {
    S3Client::new(creds(None), "us-east-1")
        .with_endpoint("http://minio:9000/", true)
        .with_http_client(fake.http())
}

// Addressing

/// On AWS the bucket goes in the host name, and the request is signed for the client's region.
#[test]
fn virtual_hosted_style_by_default() {
    let fake = Fake::new(vec![ok(200, b"hello")]);
    aws("eu-west-1", &fake)
        .get("mail-inbound", "inbox/k")
        .unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.method, "GET");
    assert_eq!(req.host(), "mail-inbound.s3.eu-west-1.amazonaws.com");
    assert!(req.uri.contains("/inbox/k"), "{}", req.uri);
    assert_eq!(req.scope_region(), "eu-west-1");
}

/// With a custom endpoint and path style, the bucket goes in the path instead.
#[test]
fn path_style_with_a_custom_endpoint() {
    let fake = Fake::new(vec![ok(200, b"hello")]);
    minio(&fake).get("mail", "inbox/k").unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.host(), "minio:9000");
    assert!(
        req.uri.starts_with("http://minio:9000/mail/inbox/k"),
        "{}",
        req.uri
    );
}

// Ranged reads

/// A ranged read asks for exactly those bytes, inclusive at both ends.
#[test]
fn get_range_sends_a_range_header() {
    let fake = Fake::new(vec![with_headers(
        206,
        &[("content-range", "bytes 5-9/20")],
        b"56789",
    )]);
    assert_eq!(minio(&fake).get_range("mail", "k", 5, 9).unwrap(), b"56789");
    assert_eq!(fake.sent()[0].header("range"), Some("bytes=5-9"));
}

/// A 416 means the range starts past the end, which for me is just an empty read.
#[test]
fn get_range_past_the_end_is_empty_not_an_error() {
    let fake = Fake::new(vec![with_headers(
        416,
        &[("content-range", "bytes */5")],
        &error_doc("InvalidRange", "The requested range is not satisfiable"),
    )]);
    assert_eq!(minio(&fake).get_range("mail", "k", 100, 200).unwrap(), b"");
}

/// An empty range doesn't cost a request.
#[test]
fn get_range_with_end_before_start_is_empty_without_a_request() {
    let fake = Fake::new(vec![]);
    assert_eq!(minio(&fake).get_range("mail", "k", 9, 3).unwrap(), b"");
    assert!(fake.sent().is_empty());
}

/// A server that sends the whole object still gives the caller only the range asked for.
#[test]
fn get_range_clamps_when_the_server_ignores_the_range() {
    // A 200 with no Content-Range is the whole object; I cut the range out of it.
    let fake = Fake::new(vec![
        ok(200, b"hello world"),
        ok(200, b"hello world"),
        ok(200, b"hello"),
    ]);
    let c = minio(&fake);
    assert_eq!(c.get_range("mail", "k", 6, 99).unwrap(), b"world");
    assert_eq!(c.get_range("mail", "k", 0, 4).unwrap(), b"hello");
    assert_eq!(c.get_range("mail", "k", 10, 20).unwrap(), b"");
}

/// A 206 carrying more than was asked for gets cut down to the range.
#[test]
fn get_range_trims_an_oversized_partial_answer() {
    let fake = Fake::new(vec![with_headers(
        206,
        &[("content-range", "bytes 0-9/10")],
        b"0123456789",
    )]);
    assert_eq!(minio(&fake).get_range("mail", "k", 0, 3).unwrap(), b"0123");
}

/// A 206 that keeps going gets read only to the range plus slack.
#[test]
fn get_range_stops_reading_a_partial_answer_past_its_slack() {
    // A server that answers a 4-byte range with far more than that: I stop reading once past
    // the range plus slack, so the body breaking afterwards never surfaces.
    let big = vec![b'x'; (4 + RANGE_SLACK + 100) as usize];
    let fake = Fake::new(vec![then_fail(
        206,
        &[("content-range", "bytes 0-3/99999")],
        big,
    )]);
    assert_eq!(minio(&fake).get_range("mail", "k", 0, 3).unwrap(), b"xxxx");
}

/// A 200 that streams the whole object gets read only up to the end of the range.
#[test]
fn get_range_on_a_200_stops_reading_once_it_has_the_range() {
    // A server that ignores Range and streams the whole object: I only need bytes up to `end`.
    let fake = Fake::new(vec![then_fail(
        200,
        &[],
        b"hello world, and a lot more".to_vec(),
    )]);
    assert_eq!(
        minio(&fake).get_range("mail", "k", 6, 10).unwrap(),
        b"world"
    );
}

/// A body that breaks before I have what I need is a transport error.
#[test]
fn a_body_that_breaks_before_the_limit_is_a_transport_error() {
    // The positive control for the two tests above: when the break comes before I have
    // enough, it does surface.
    let fake = Fake::new(vec![then_fail(200, &[], b"short".to_vec())]);
    assert!(matches!(
        minio(&fake).get("mail", "k"),
        Err(S3Error::Transport(_))
    ));
}

// The size cap on a whole get

/// A whole get returns the body and sends no Range header.
#[test]
fn get_returns_the_body() {
    let fake = Fake::new(vec![ok(200, b"From: a@example.com\r\n\r\nhi")]);
    assert_eq!(
        minio(&fake).get("mail", "k").unwrap(),
        b"From: a@example.com\r\n\r\nhi"
    );
    assert_eq!(fake.sent()[0].header("range"), None);
}

/// A Content-Length over MAX_GET_BYTES is refused as TooLarge before any body is read.
#[test]
fn get_of_an_object_over_the_cap_is_too_large() {
    let fake = Fake::new(vec![with_headers(
        200,
        &[("content-length", "52428800")],
        b"",
    )]);
    assert_eq!(
        minio(&fake).get("mail", "k").unwrap_err(),
        S3Error::TooLarge {
            size: Some(52_428_800),
            limit: MAX_GET_BYTES,
        }
    );
}

/// A body with no length that runs past the cap is cut off at the cap and refused.
#[test]
fn get_stops_reading_a_body_that_runs_past_the_cap() {
    // No trustworthy length up front: the body itself runs over, and I stop at the cap.
    let body = vec![b'x'; (MAX_GET_BYTES + 10) as usize];
    let fake = Fake::new(vec![then_fail(200, &[], body)]);
    match minio(&fake).get("mail", "k").unwrap_err() {
        S3Error::TooLarge { limit, .. } => assert_eq!(limit, MAX_GET_BYTES),
        other => panic!("{other:?}"),
    }
}

/// The TooLarge message gives the object's size and the limit, so the user sees by how much.
#[test]
fn the_too_large_message_names_both_sizes() {
    let text = S3Error::TooLarge {
        size: Some(52_428_800),
        limit: MAX_GET_BYTES,
    }
    .to_string();
    assert!(text.contains("52428800 bytes"), "{text}");
    assert!(text.contains(&MAX_GET_BYTES.to_string()), "{text}");
}

// Empty keys

/// An empty key would address the bucket itself, so every call refuses it up front.
#[test]
fn empty_keys_are_refused_without_a_request() {
    let fake = Fake::new(vec![]);
    let c = minio(&fake);
    assert_eq!(c.get("mail", "").unwrap_err(), S3Error::EmptyKey);
    assert_eq!(
        c.get_range("mail", "", 0, 9).unwrap_err(),
        S3Error::EmptyKey
    );
    assert_eq!(c.delete("mail", "").unwrap_err(), S3Error::EmptyKey);
    assert_eq!(
        c.put_object("mail", "", b"x").unwrap_err(),
        S3Error::EmptyKey
    );
    assert!(fake.sent().is_empty());
}

// Listings

/// ListBuckets parses every bucket and its creation date, signed for the client's region.
#[test]
fn list_buckets_reads_every_bucket() {
    let fake = Fake::new(vec![ok(200, &fixture("list_buckets.xml"))]);
    let buckets = aws("ap-southeast-2", &fake).list_buckets().unwrap();
    assert_eq!(
        buckets.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["mail-inbound", "quotes.example"]
    );
    assert!(buckets[0].created.is_some());
    assert_eq!(fake.sent()[0].scope_region(), "ap-southeast-2");
}

/// A listing is a ListObjectsV2 call with every parameter encoded, and asks for url-encoded keys.
#[test]
fn list_sends_a_list_objects_v2_query() {
    let fake = Fake::new(vec![ok(200, &fixture("list_objects_v2_folders.xml"))]);
    minio(&fake)
        .with_max_keys(4)
        .list("mail", "inbox/", Some("/"), Some("tok+/="))
        .unwrap();
    let uri = &fake.sent()[0].uri;
    for part in [
        "list-type=2",
        "prefix=inbox%2F",
        "delimiter=%2F",
        "continuation-token=tok%2B%2F%3D",
        "encoding-type=url",
        "max-keys=4",
    ] {
        assert!(uri.contains(part), "{part} missing from {uri}");
    }
}

/// Folders, sizes, dates and the next token all come through, and url-encoded keys get decoded.
#[test]
fn list_decodes_keys_when_the_response_is_url_encoded() {
    let fake = Fake::new(vec![ok(200, &fixture("list_objects_v2_folders.xml"))]);
    let l = minio(&fake)
        .list("mail", "inbox/", Some("/"), None)
        .unwrap();
    assert_eq!(l.prefixes, vec!["inbox/2024/", "inbox/spam folder/"]);
    assert_eq!(l.objects[0].key, "inbox/0a1b2c3d4e5f");
    assert_eq!(l.objects[0].size, 48213);
    assert!(l.objects[0].last_modified.is_some());
    // Keys go up raw: `+` is a space and `%2B` a plus once decoded, and nothing is escaped
    // for display here. The screens do that.
    assert_eq!(l.objects[1].key, "inbox/a b+cé%.eml");
    assert_eq!(
        l.next_token.as_deref(),
        Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
    );
}

/// A server that ignored encoding-type sends raw keys, and I don't decode those a second time.
#[test]
fn list_leaves_keys_alone_when_the_server_ignored_encoding_type() {
    let body = br#"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>a+b%41</Key><Size>1</Size></Contents></ListBucketResult>"#;
    let fake = Fake::new(vec![ok(200, body)]);
    let l = minio(&fake).list("mail", "", None, None).unwrap();
    assert_eq!(l.objects[0].key, "a+b%41");
}

/// Control characters in a key come through as they are, since escaping them is display work.
#[test]
fn keys_with_control_characters_come_up_raw() {
    // An escape sequence in a key is data. It reaches the screens as is, and they escape it.
    let body = br#"<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated><Contents><Key>inbox%2F%1B%5B31mred</Key><Size>1</Size></Contents></ListBucketResult>"#;
    let fake = Fake::new(vec![ok(200, body)]);
    let l = minio(&fake).list("mail", "", None, None).unwrap();
    assert_eq!(l.objects[0].key, "inbox/\u{1b}[31mred");
}

/// The last page ends the listing even if the server sent a token with it.
#[test]
fn a_page_that_says_it_is_not_truncated_has_no_next_token() {
    // Panel item N23: a stray NextContinuationToken on the last page must not keep paging.
    let body = br#"<ListBucketResult><IsTruncated>false</IsTruncated><NextContinuationToken>stray</NextContinuationToken><Contents><Key>a</Key><Size>1</Size></Contents></ListBucketResult>"#;
    let fake = Fake::new(vec![ok(200, body)]);
    let l = minio(&fake).list("mail", "", None, None).unwrap();
    assert_eq!(l.next_token, None);
}

/// A truncated page passes its token on, so paging carries on.
#[test]
fn a_truncated_page_keeps_its_token() {
    let body = br#"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>more</NextContinuationToken><Contents><Key>a</Key><Size>1</Size></Contents></ListBucketResult>"#;
    let fake = Fake::new(vec![ok(200, body)]);
    let l = minio(&fake).list("mail", "", None, None).unwrap();
    assert_eq!(l.next_token.as_deref(), Some("more"));
}

// Errors

/// An S3 error document becomes a Service error with its status, code and message.
#[test]
fn error_documents_become_service_errors() {
    let fake = Fake::new(vec![ok(
        404,
        &error_doc("NoSuchKey", "The specified key does not exist."),
    )]);
    let err = minio(&fake).get("mail", "inbox/missing").unwrap_err();
    assert_eq!(
        err,
        S3Error::Service {
            status: 404,
            code: "NoSuchKey".into(),
            message: "The specified key does not exist.".into(),
        }
    );
    assert!(err.is_not_found());
}

/// Delete is a DELETE on the object's own path.
#[test]
fn delete_sends_delete() {
    let fake = Fake::new(vec![ok(204, b"")]);
    minio(&fake).delete("mail", "inbox/k").unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.method, "DELETE");
    assert!(
        req.uri.starts_with("http://minio:9000/mail/inbox/k"),
        "{}",
        req.uri
    );
}

/// Nothing listening is a transport error, not a service error.
#[test]
fn an_unreachable_endpoint_is_a_transport_error() {
    let c = S3Client::new(creds(None), "us-east-1").with_endpoint("http://127.0.0.1:9", true);
    assert!(matches!(c.list_buckets(), Err(S3Error::Transport(_))));
}

// Secrets

/// A client's Debug shows its region but never the secret or the token.
#[test]
fn secrets_never_reach_debug_output() {
    let c = S3Client::new(creds(Some(TOKEN)), "us-east-1");
    let dbg = format!("{c:?}");
    assert!(!dbg.contains(SECRET), "{dbg}");
    assert!(!dbg.contains(TOKEN), "{dbg}");
    assert!(dbg.contains("us-east-1"), "{dbg}");
}

/// A server that echoes the secret or token back (raw, or url-encoded in either case of hex)
/// doesn't get it into the error's message or Debug.
#[test]
fn secrets_are_scrubbed_from_errors() {
    // A server that echoes a secret back, raw or url-encoded, must not get it shown.
    let enc = |s: &str| {
        s.replace('/', "%2F")
            .replace('+', "%2B")
            .replace('=', "%3D")
    };
    // The same escapes with lower-case hex digits, which some servers write.
    let low = |s: &str| {
        enc(s)
            .replace("%2F", "%2f")
            .replace("%2B", "%2b")
            .replace("%3D", "%3d")
    };
    let fake = Fake::new(vec![
        ok(
            403,
            &error_doc("SignatureDoesNotMatch", &format!("{SECRET} {TOKEN}")),
        ),
        ok(
            403,
            &error_doc(
                "SignatureDoesNotMatch",
                &format!("{} {}", enc(SECRET), enc(TOKEN)),
            ),
        ),
        ok(
            403,
            &error_doc(
                "SignatureDoesNotMatch",
                &format!("{} {}", low(SECRET), low(TOKEN)),
            ),
        ),
    ]);
    let c = S3Client::new(creds(Some(TOKEN)), "us-east-1").with_http_client(fake.http());
    for _ in 0..3 {
        let err = c.get("mail", "k").unwrap_err();
        for text in [err.to_string(), format!("{err:?}")] {
            for s in [SECRET, TOKEN] {
                for form in [s.to_string(), enc(s), low(s)] {
                    assert!(!text.contains(&form), "{form} in {text}");
                }
            }
        }
    }
}

/// A session token is sent as x-amz-security-token and covered by the signature.
#[test]
fn a_session_token_goes_out_as_a_signed_header() {
    let fake = Fake::new(vec![ok(200, b"x")]);
    S3Client::new(creds(Some(TOKEN)), "us-east-1")
        .with_http_client(fake.http())
        .get("mail", "k")
        .unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.header("x-amz-security-token"), Some(TOKEN));
    assert!(
        req.header("authorization")
            .unwrap()
            .contains("x-amz-security-token")
    );
}

// Cross-region buckets

/// A redirect naming the bucket's region gets the call retried there, and the client remembers
/// that region for the bucket from then on.
#[test]
fn a_301_with_a_region_header_moves_the_bucket_to_that_region() {
    let fake = Fake::new(vec![
        with_headers(
            301,
            &[("x-amz-bucket-region", "eu-west-2")],
            &error_doc("PermanentRedirect", "use the right endpoint"),
        ),
        ok(200, b"hello"),
    ]);
    let c = aws("us-east-1", &fake);
    assert_eq!(c.get("mail-inbound", "k").unwrap(), b"hello");
    let sent = fake.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].scope_region(), "us-east-1");
    assert_eq!(sent[1].host(), "mail-inbound.s3.eu-west-2.amazonaws.com");
    assert_eq!(sent[1].scope_region(), "eu-west-2");

    // The next call goes straight there, and other buckets stay put.
    fake.push(ok(204, b""));
    c.delete("mail-inbound", "k").unwrap();
    fake.push(ok(200, b"x"));
    c.get("other", "k").unwrap();
    let sent = fake.sent();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[2].scope_region(), "eu-west-2");
    assert_eq!(sent[3].scope_region(), "us-east-1");
}

/// A wrong-region error that doesn't name the region sends me to HeadBucket to find it.
#[test]
fn a_redirect_without_a_region_header_asks_head_bucket() {
    let fake = Fake::new(vec![
        ok(
            400,
            &error_doc("AuthorizationHeaderMalformed", "wrong region"),
        ),
        with_headers(301, &[("x-amz-bucket-region", "ap-northeast-1")], b""),
        ok(200, b"x"),
    ]);
    let c = aws("us-east-1", &fake);
    assert_eq!(c.get("mail-inbound", "k").unwrap(), b"x");
    let sent = fake.sent();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].method, "HEAD");
    assert_eq!(sent[2].scope_region(), "ap-northeast-1");
}

/// The region I learn is visible through the Store trait object the TUI holds.
#[test]
fn a_learned_region_is_visible_through_dyn_store() {
    let fake = Fake::new(vec![
        with_headers(301, &[("x-amz-bucket-region", "eu-west-2")], b""),
        ok(200, b"x"),
    ]);
    let store: Arc<dyn Store> = Arc::new(aws("us-east-1", &fake));
    assert_eq!(store.bucket_region("mail-inbound"), None);
    store.get("mail-inbound", "k").unwrap();
    assert_eq!(
        store.bucket_region("mail-inbound").as_deref(),
        Some("eu-west-2")
    );
    assert_eq!(store.bucket_region("other"), None);
}

/// A region header that isn't a region name is never put into a host, so the redirect is
/// just an error.
#[test]
fn a_bad_region_hint_is_ignored() {
    let fake = Fake::new(vec![
        with_headers(301, &[("x-amz-bucket-region", "x.attacker.example/#")], b""),
        with_headers(301, &[("x-amz-bucket-region", "x.attacker.example/#")], b""),
    ]);
    let c = aws("us-east-1", &fake);
    assert!(matches!(
        c.get("mail-inbound", "k"),
        Err(S3Error::Service { status: 301, .. })
    ));
    assert_eq!(c.bucket_region("mail-inbound"), None);
    assert!(fake.sent().iter().all(|r| !r.uri.contains("attacker")));
}

/// Only a wrong-region 400 counts as a redirect. Any other 400 is returned without a retry.
#[test]
fn other_400s_are_not_redirects() {
    let fake = Fake::new(vec![with_headers(
        400,
        &[("x-amz-bucket-region", "eu-west-2")],
        &error_doc("InvalidArgument", "nope"),
    )]);
    let err = aws("us-east-1", &fake)
        .get("mail-inbound", "k")
        .unwrap_err();
    assert!(
        matches!(err, S3Error::Service { status: 400, ref code, .. } if code == "InvalidArgument")
    );
    assert_eq!(fake.sent().len(), 1);
}

// Credentials and regions from profile files

/// Writes `name` under `dir` and hands back its path.
fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// A profile's keys, token and region from the files are what the request is signed with.
#[test]
fn a_profile_with_static_keys_and_a_token_signs_with_them() {
    let dir = tempfile::tempdir().unwrap();
    let config = write(
        dir.path(),
        "config",
        "[profile work]\nregion = eu-central-1\n",
    );
    let credentials = write(
        dir.path(),
        "credentials",
        &format!(
            "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = nope\n\
             [work]\naws_access_key_id = AKIDWORK\naws_secret_access_key = {SECRET}\naws_session_token = {TOKEN}\n"
        ),
    );
    let fake = Fake::new(vec![ok(200, b"x")]);
    let c = S3Client::from_profile_files("work", None, &config, &credentials)
        .with_http_client(fake.http());
    assert_eq!(c.region(), "eu-central-1");
    c.get("mail-inbound", "k").unwrap();
    let req = &fake.sent()[0];
    assert!(
        req.header("authorization")
            .unwrap()
            .contains("Credential=AKIDWORK/")
    );
    assert_eq!(req.header("x-amz-security-token"), Some(TOKEN));
    assert_eq!(req.scope_region(), "eu-central-1");
}

/// A valid hint wins over the profile's region, a bad one is ignored, and with neither I end
/// up in us-east-1.
#[test]
fn a_region_hint_beats_the_profile_and_a_bad_one_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let config = write(
        dir.path(),
        "config",
        "[profile work]\nregion = eu-central-1\n",
    );
    let credentials = write(
        dir.path(),
        "credentials",
        "[work]\naws_access_key_id = AKIDWORK\naws_secret_access_key = s\n",
    );
    let hinted = S3Client::from_profile_files("work", Some("ca-central-1"), &config, &credentials);
    assert_eq!(hinted.region(), "ca-central-1");
    let bad = S3Client::from_profile_files("work", Some("evil.example/#"), &config, &credentials);
    assert_eq!(bad.region(), "eu-central-1");
    let none = write(dir.path(), "config-empty", "");
    assert_eq!(
        S3Client::from_profile_files("work", None, &none, &credentials).region(),
        "us-east-1"
    );
}

/// A profile whose keys come from credential_process runs the process and signs with its output.
#[test]
fn credential_process_supplies_the_keys() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("creds.sh");
    let mut f = std::fs::File::create(&script).unwrap();
    writeln!(
        f,
        "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"AKIDPROCESS\", \"SecretAccessKey\": \"process-secret\", \"SessionToken\": \"process-token\"}}'"
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let config = write(
        dir.path(),
        "config",
        &format!(
            "[profile proc]\nregion = us-west-2\ncredential_process = {}\n",
            script.display()
        ),
    );
    let credentials = write(dir.path(), "credentials", "");
    let fake = Fake::new(vec![ok(200, b"x")]);
    S3Client::from_profile_files("proc", None, &config, &credentials)
        .with_http_client(fake.http())
        .get("mail-inbound", "k")
        .unwrap();
    let req = &fake.sent()[0];
    assert!(
        req.header("authorization")
            .unwrap()
            .contains("Credential=AKIDPROCESS/")
    );
    assert_eq!(req.header("x-amz-security-token"), Some("process-token"));
}

/// A missing profile fails without a request, names itself, and leaks nothing from the others.
#[test]
fn a_missing_profile_is_an_error_that_names_it_and_hides_other_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let config = write(dir.path(), "config", "");
    let credentials = write(
        dir.path(),
        "credentials",
        &format!("[other]\naws_access_key_id = AKIDOTHER\naws_secret_access_key = {SECRET}\n"),
    );
    let fake = Fake::new(vec![]);
    let err = S3Client::from_profile_files("ghost", None, &config, &credentials)
        .with_http_client(fake.http())
        .list_buckets()
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(text.contains("ghost"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(fake.sent().is_empty());
}

/// A client built from profile files names its profile in Debug but shows no secret.
#[test]
fn a_profile_client_hides_its_secrets_from_debug() {
    let dir = tempfile::tempdir().unwrap();
    let config = write(dir.path(), "config", "");
    let credentials = write(
        dir.path(),
        "credentials",
        &format!(
            "[work]\naws_access_key_id = AKIDWORK\naws_secret_access_key = {SECRET}\naws_session_token = {TOKEN}\n"
        ),
    );
    let c = S3Client::from_profile_files("work", None, &config, &credentials);
    let dbg = format!("{c:?}");
    assert!(dbg.contains("work"), "{dbg}");
    assert!(!dbg.contains(SECRET) && !dbg.contains(TOKEN), "{dbg}");
}

/// A range that starts beyond the cap, on a server that sends everything, is TooLarge.
#[test]
fn a_range_past_the_cap_on_a_server_that_ignores_range_is_too_large() {
    // The server streams the whole object and the range starts beyond what I'll read of it,
    // so the honest answer is TooLarge, not an empty range.
    let start = MAX_GET_BYTES + 100;
    let body = vec![b'x'; (MAX_GET_BYTES + 200) as usize];
    let fake = Fake::new(vec![then_fail(200, &[], body)]);
    match minio(&fake)
        .get_range("mail", "k", start, start + 9)
        .unwrap_err()
    {
        S3Error::TooLarge { limit, .. } => assert_eq!(limit, MAX_GET_BYTES),
        other => panic!("{other:?}"),
    }
}
