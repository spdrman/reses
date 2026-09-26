//! `S3Client` against a scripted transport: URLs, signing inputs, ranges, errors, secrets and
//! region redirects. The redirect answers copy the shapes S3 sends (a 301 with
//! `x-amz-bucket-region`, a 400 `AuthorizationHeaderMalformed` carrying `<Region>`, a 400
//! `PermanentRedirect`), since MinIO never redirects.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use reses::s3::{Credentials, HttpRequest, HttpResponse, S3Client, S3Error, Store, Transport};

const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEXAMPLE/session+token==";

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/s3/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

type Answer = Result<HttpResponse, String>;

/// Answers requests from a queue and records what it was sent.
#[derive(Default)]
struct Fake {
    answers: Mutex<VecDeque<Answer>>,
    sent: Mutex<Vec<HttpRequest>>,
}

impl Fake {
    fn new(answers: Vec<Answer>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            sent: Mutex::default(),
        })
    }
    fn sent(&self) -> Vec<HttpRequest> {
        self.sent.lock().unwrap().clone()
    }
    fn push(&self, a: Answer) {
        self.answers.lock().unwrap().push_back(a);
    }
}

impl Transport for Fake {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        self.sent.lock().unwrap().push(req.clone());
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected request {req:?}"))
    }
}

fn ok(status: u16, body: &[u8]) -> Answer {
    Ok(HttpResponse {
        status,
        headers: vec![],
        body: body.to_vec(),
    })
}

fn with_headers(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    Ok(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.to_vec(),
    })
}

fn creds(token: Option<&str>) -> Credentials {
    Credentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_access_key: SECRET.into(),
        session_token: token.map(str::to_string),
    }
}

fn aws(region: &str, fake: &Arc<Fake>) -> S3Client {
    S3Client::new(creds(None), region).with_transport(fake.clone())
}

fn minio(fake: &Arc<Fake>) -> S3Client {
    S3Client::new(creds(None), "us-east-1")
        .with_endpoint("http://minio:9000/", true)
        .with_transport(fake.clone())
}

fn scope_region(req: &HttpRequest) -> String {
    let auth = req.header("authorization").expect("signed");
    let cred = auth.split("Credential=").nth(1).unwrap();
    cred.split('/').nth(2).unwrap().to_string()
}

const WEIRD_KEY: &str = "inbox/a b+c/é%.eml";
const WEIRD_PATH: &str = "inbox/a%20b%2Bc/%C3%A9%25.eml";

// URLs and addressing

#[test]
fn virtual_hosted_style_by_default() {
    let fake = Fake::new(vec![ok(200, b"hello")]);
    aws("eu-west-1", &fake)
        .get("mail-inbound", WEIRD_KEY)
        .unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.method, "GET");
    assert_eq!(
        req.url,
        format!("https://mail-inbound.s3.eu-west-1.amazonaws.com/{WEIRD_PATH}")
    );
    assert_eq!(scope_region(req), "eu-west-1");
}

#[test]
fn path_style_with_a_custom_endpoint() {
    let fake = Fake::new(vec![ok(200, b"hello")]);
    minio(&fake).get("mail", WEIRD_KEY).unwrap();
    assert_eq!(
        fake.sent()[0].url,
        format!("http://minio:9000/mail/{WEIRD_PATH}")
    );
}

#[test]
fn virtual_hosted_style_with_a_custom_endpoint() {
    let fake = Fake::new(vec![ok(200, b"x")]);
    S3Client::new(creds(None), "us-east-1")
        .with_endpoint("https://s3.example.test", false)
        .with_transport(fake.clone())
        .get("mail", "k")
        .unwrap();
    assert_eq!(fake.sent()[0].url, "https://mail.s3.example.test/k");
}

#[test]
fn dotted_bucket_names_fall_back_to_path_style_on_aws() {
    // A dot in the bucket breaks the *.s3 wildcard certificate, so AWS clients use the path.
    let fake = Fake::new(vec![ok(200, b"x")]);
    aws("us-east-1", &fake).get("quotes.example", "k").unwrap();
    assert_eq!(
        fake.sent()[0].url,
        "https://s3.us-east-1.amazonaws.com/quotes.example/k"
    );
}

#[test]
fn keys_keep_slashes_and_leading_or_doubled_ones() {
    let fake = Fake::new(vec![ok(200, b"x")]);
    minio(&fake).get("mail", "/a//b/").unwrap();
    assert_eq!(fake.sent()[0].url, "http://minio:9000/mail//a//b/");
}

// Operations

#[test]
fn list_buckets_hits_the_service_endpoint() {
    let fake = Fake::new(vec![ok(200, &fixture("list_buckets.xml"))]);
    let buckets = aws("ap-southeast-2", &fake).list_buckets().unwrap();
    assert_eq!(
        buckets.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["mail-inbound", "quotes.example"]
    );
    let req = &fake.sent()[0];
    assert_eq!(req.method, "GET");
    assert_eq!(req.url, "https://s3.ap-southeast-2.amazonaws.com/");
}

#[test]
fn list_sends_a_list_objects_v2_query_and_decodes_keys() {
    let fake = Fake::new(vec![ok(200, &fixture("list_objects_v2_folders.xml"))]);
    let l = aws("us-east-1", &fake)
        .with_max_keys(4)
        .list("mail-inbound", "inbox/", Some("/"), Some("tok+/="))
        .unwrap();
    let req = &fake.sent()[0];
    assert_eq!(
        req.url,
        "https://mail-inbound.s3.us-east-1.amazonaws.com/?continuation-token=tok%2B%2F%3D\
         &delimiter=%2F&encoding-type=url&list-type=2&max-keys=4&prefix=inbox%2F"
    );
    assert_eq!(l.prefixes, vec!["inbox/2024/", "inbox/spam folder/"]);
    assert_eq!(l.objects[1].key, "inbox/a b+cé%.eml");
    assert!(l.next_token.is_some());
}

#[test]
fn list_without_delimiter_or_token_leaves_them_out() {
    let fake = Fake::new(vec![ok(200, &fixture("list_objects_v2_empty.xml"))]);
    minio(&fake).list("mail", "", None, None).unwrap();
    assert_eq!(
        fake.sent()[0].url,
        "http://minio:9000/mail?encoding-type=url&list-type=2&prefix="
    );
}

#[test]
fn get_range_sends_a_range_header() {
    let fake = Fake::new(vec![ok(206, b"56789")]);
    let data = minio(&fake).get_range("mail", "k", 5, 9).unwrap();
    assert_eq!(data, b"56789");
    let req = &fake.sent()[0];
    assert_eq!(req.header("range"), Some("bytes=5-9"));
    // Range is part of what gets signed.
    assert!(
        req.header("authorization")
            .unwrap()
            .contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,")
    );
}

#[test]
fn get_range_past_the_end_is_empty_not_an_error() {
    let fake = Fake::new(vec![with_headers(
        416,
        &[("content-range", "bytes */5")],
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>InvalidRange</Code><Message>The requested range is not satisfiable</Message></Error>",
    )]);
    assert_eq!(minio(&fake).get_range("mail", "k", 100, 200).unwrap(), b"");
}

#[test]
fn get_range_with_end_before_start_is_empty_without_a_request() {
    let fake = Fake::new(vec![]);
    assert_eq!(minio(&fake).get_range("mail", "k", 9, 3).unwrap(), b"");
    assert!(fake.sent().is_empty());
}

#[test]
fn get_range_clamps_when_the_server_ignores_the_range() {
    // Some S3-compatible servers answer 200 with the whole object.
    let fake = Fake::new(vec![ok(200, b"hello world"), ok(200, b"hello")]);
    let c = minio(&fake);
    assert_eq!(c.get_range("mail", "k", 6, 99).unwrap(), b"world");
    assert_eq!(c.get_range("mail", "k", 10, 20).unwrap(), b"");
}

#[test]
fn get_range_clamps_an_oversized_partial_answer() {
    let fake = Fake::new(vec![ok(206, b"0123456789")]);
    assert_eq!(minio(&fake).get_range("mail", "k", 0, 3).unwrap(), b"0123");
}

#[test]
fn get_returns_the_body() {
    let fake = Fake::new(vec![ok(200, b"From: a@b\r\n\r\nhi")]);
    assert_eq!(
        minio(&fake).get("mail", "k").unwrap(),
        b"From: a@b\r\n\r\nhi"
    );
    assert_eq!(fake.sent()[0].header("range"), None);
}

#[test]
fn delete_sends_delete() {
    let fake = Fake::new(vec![ok(204, b"")]);
    minio(&fake).delete("mail", WEIRD_KEY).unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.method, "DELETE");
    assert_eq!(req.url, format!("http://minio:9000/mail/{WEIRD_PATH}"));
}

#[test]
fn every_request_carries_date_and_payload_hash() {
    let fake = Fake::new(vec![ok(204, b"")]);
    minio(&fake).delete("mail", "k").unwrap();
    let req = &fake.sent()[0];
    assert_eq!(
        req.header("x-amz-content-sha256"),
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
    let date = req.header("x-amz-date").unwrap();
    assert_eq!(date.len(), 16, "{date}");
    assert!(date.ends_with('Z'));
    assert!(
        req.header("authorization")
            .unwrap()
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/")
    );
}

#[test]
fn session_token_goes_out_as_a_signed_header() {
    let fake = Fake::new(vec![ok(200, b"x")]);
    S3Client::new(creds(Some(TOKEN)), "us-east-1")
        .with_transport(fake.clone())
        .get("mail", "k")
        .unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.header("x-amz-security-token"), Some(TOKEN));
    assert!(
        req.header("authorization")
            .unwrap()
            .contains(";x-amz-security-token,")
    );
}

#[test]
fn put_object_signs_the_body_hash() {
    let fake = Fake::new(vec![ok(200, b"")]);
    minio(&fake)
        .put_object("mail", "k", b"Welcome to Amazon S3.")
        .unwrap();
    let req = &fake.sent()[0];
    assert_eq!(req.method, "PUT");
    assert_eq!(req.body, b"Welcome to Amazon S3.");
    assert_eq!(
        req.header("x-amz-content-sha256"),
        Some("44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072")
    );
}

#[test]
fn create_bucket_outside_us_east_1_sends_a_location_constraint() {
    let fake = Fake::new(vec![ok(200, b""), ok(200, b"")]);
    minio(&fake).create_bucket("mail").unwrap();
    assert_eq!(fake.sent()[0].method, "PUT");
    assert_eq!(fake.sent()[0].url, "http://minio:9000/mail");
    assert!(fake.sent()[0].body.is_empty());

    aws("eu-west-2", &fake).create_bucket("mail").unwrap();
    let body = String::from_utf8(fake.sent()[1].body.clone()).unwrap();
    assert!(
        body.contains("<LocationConstraint>eu-west-2</LocationConstraint>"),
        "{body}"
    );
}

// Errors

#[test]
fn error_documents_become_service_errors() {
    let fake = Fake::new(vec![ok(404, &fixture("error_no_such_key.xml"))]);
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

#[test]
fn an_error_without_a_document_still_carries_the_status() {
    let fake = Fake::new(vec![ok(403, b""), ok(503, b"<html>busy</html>")]);
    let c = minio(&fake);
    match c.get("mail", "k").unwrap_err() {
        S3Error::Service { status: 403, .. } => {}
        other => panic!("{other:?}"),
    }
    match c.list_buckets().unwrap_err() {
        S3Error::Service { status: 503, .. } => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn network_failures_become_transport_errors() {
    let fake = Fake::new(vec![Err("connection refused".into())]);
    assert_eq!(
        minio(&fake).get("mail", "k").unwrap_err(),
        S3Error::Transport("connection refused".into())
    );
}

#[test]
fn malformed_xml_becomes_a_parse_error() {
    let fake = Fake::new(vec![
        ok(200, &fixture("malformed_truncated.xml")),
        ok(200, b"<ListAllMyBucketsResult><Buckets><Bucket><Name>x"),
    ]);
    let c = minio(&fake);
    assert!(matches!(
        c.list("mail", "", None, None),
        Err(S3Error::Parse(_))
    ));
    assert!(matches!(c.list_buckets(), Err(S3Error::Parse(_))));
}

// Secrets

#[test]
fn secrets_never_reach_debug_output() {
    let c = S3Client::new(creds(Some(TOKEN)), "us-east-1");
    let dbg = format!("{c:?}");
    assert!(!dbg.contains(SECRET), "{dbg}");
    assert!(!dbg.contains(TOKEN), "{dbg}");

    let fake = Fake::new(vec![ok(200, b"x")]);
    S3Client::new(creds(Some(TOKEN)), "us-east-1")
        .with_transport(fake.clone())
        .get("mail", "k")
        .unwrap();
    let dbg = format!("{:?}", fake.sent()[0]);
    assert!(!dbg.contains(TOKEN), "{dbg}");
    assert!(!dbg.contains("Signature="), "{dbg}");
    assert!(dbg.contains("mail"), "{dbg}");
}

#[test]
fn secrets_are_scrubbed_from_errors() {
    // A transport error or an error document that echoes a secret back must not show it.
    let echo = format!(
        "<Error><Code>SignatureDoesNotMatch</Code><Message>bad sig for {SECRET} with {TOKEN}</Message></Error>"
    );
    let fake = Fake::new(vec![
        Err(format!("tls failure near {SECRET} and {TOKEN}")),
        ok(403, echo.as_bytes()),
        ok(
            200,
            format!("<ListAllMyBucketsResult><oops {TOKEN}").as_bytes(),
        ),
    ]);
    let c = S3Client::new(creds(Some(TOKEN)), "us-east-1").with_transport(fake.clone());
    for err in [
        c.get("mail", "k").unwrap_err(),
        c.get("mail", "k").unwrap_err(),
        c.list_buckets().unwrap_err(),
    ] {
        for text in [err.to_string(), format!("{err:?}")] {
            assert!(!text.contains(SECRET), "{text}");
            assert!(!text.contains(TOKEN), "{text}");
        }
    }
}

// Region redirects

#[test]
fn a_301_with_a_region_header_retries_there_and_remembers_it() {
    let fake = Fake::new(vec![
        with_headers(
            301,
            &[("x-amz-bucket-region", "eu-west-2")],
            &fixture("error_permanent_redirect.xml"),
        ),
        ok(200, b"hello"),
    ]);
    let c = aws("us-east-1", &fake);
    assert_eq!(c.get("mail-inbound", "k").unwrap(), b"hello");
    let sent = fake.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(scope_region(&sent[0]), "us-east-1");
    assert_eq!(
        sent[1].url,
        "https://mail-inbound.s3.eu-west-2.amazonaws.com/k"
    );
    assert_eq!(scope_region(&sent[1]), "eu-west-2");
    assert_eq!(
        c.bucket_region("mail-inbound").as_deref(),
        Some("eu-west-2")
    );

    // The next call goes straight to the right region.
    fake.push(ok(204, b""));
    c.delete("mail-inbound", "k").unwrap();
    let sent = fake.sent();
    assert_eq!(sent.len(), 3);
    assert_eq!(scope_region(&sent[2]), "eu-west-2");
    assert_eq!(
        sent[2].url,
        "https://mail-inbound.s3.eu-west-2.amazonaws.com/k"
    );

    // Other buckets are not affected.
    fake.push(ok(200, b"x"));
    c.get("other", "k").unwrap();
    assert_eq!(scope_region(&fake.sent()[3]), "us-east-1");
}

#[test]
fn a_400_authorization_header_malformed_with_a_region_in_the_body_retries() {
    let fake = Fake::new(vec![
        ok(400, &fixture("error_authorization_header_malformed.xml")),
        ok(200, &fixture("list_objects_v2_last_page.xml")),
    ]);
    let c = aws("us-east-1", &fake);
    let l = c.list("mail-inbound", "inbox/", Some("/"), None).unwrap();
    assert_eq!(l.objects[0].key, "inbox/zz-last");
    assert_eq!(scope_region(&fake.sent()[1]), "eu-west-2");
    assert_eq!(
        c.bucket_region("mail-inbound").as_deref(),
        Some("eu-west-2")
    );
}

#[test]
fn a_400_permanent_redirect_with_a_region_header_retries() {
    let fake = Fake::new(vec![
        with_headers(
            400,
            &[("X-Amz-Bucket-Region", "ap-northeast-1")],
            &fixture("error_permanent_redirect.xml"),
        ),
        ok(206, b"From"),
    ]);
    let c = aws("us-east-1", &fake);
    assert_eq!(c.get_range("mail-inbound", "k", 0, 3).unwrap(), b"From");
    let retry = &fake.sent()[1];
    assert_eq!(scope_region(retry), "ap-northeast-1");
    assert_eq!(retry.header("range"), Some("bytes=0-3"));
}

#[test]
fn redirects_on_a_custom_endpoint_change_only_the_signing_region() {
    let fake = Fake::new(vec![
        ok(400, &fixture("error_authorization_header_malformed.xml")),
        ok(200, b"x"),
    ]);
    minio(&fake).get("mail", "k").unwrap();
    let retry = &fake.sent()[1];
    assert_eq!(retry.url, "http://minio:9000/mail/k");
    assert_eq!(scope_region(retry), "eu-west-2");
}

#[test]
fn only_one_retry() {
    let fake = Fake::new(vec![
        with_headers(301, &[("x-amz-bucket-region", "eu-west-2")], b""),
        with_headers(301, &[("x-amz-bucket-region", "eu-west-3")], b""),
    ]);
    let err = aws("us-east-1", &fake)
        .get("mail-inbound", "k")
        .unwrap_err();
    assert!(
        matches!(err, S3Error::Service { status: 301, .. }),
        "{err:?}"
    );
    assert_eq!(fake.sent().len(), 2);
}

#[test]
fn a_redirect_without_a_region_hint_is_an_error() {
    let fake = Fake::new(vec![ok(301, &fixture("error_permanent_redirect.xml"))]);
    let err = aws("us-east-1", &fake)
        .get("mail-inbound", "k")
        .unwrap_err();
    match err {
        S3Error::Service { status, code, .. } => {
            assert_eq!((status, code.as_str()), (301, "PermanentRedirect"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.sent().len(), 1);
}

#[test]
fn other_400s_do_not_retry_even_with_a_region_header() {
    let fake = Fake::new(vec![with_headers(
        400,
        &[("x-amz-bucket-region", "eu-west-2")],
        b"<Error><Code>InvalidArgument</Code><Message>nope</Message></Error>",
    )]);
    let err = aws("us-east-1", &fake)
        .get("mail-inbound", "k")
        .unwrap_err();
    assert!(
        matches!(err, S3Error::Service { status: 400, ref code, .. } if code == "InvalidArgument")
    );
    assert_eq!(fake.sent().len(), 1);
}

#[test]
fn a_redirect_to_the_same_region_does_not_loop() {
    let fake = Fake::new(vec![with_headers(
        301,
        &[("x-amz-bucket-region", "us-east-1")],
        b"",
    )]);
    assert!(aws("us-east-1", &fake).get("mail-inbound", "k").is_err());
    assert_eq!(fake.sent().len(), 1);
}

#[test]
fn client_is_usable_as_a_shared_store() {
    fn assert_store<T: Store + 'static>() {}
    assert_store::<S3Client>();
    let fake = Fake::new(vec![ok(200, b"x")]);
    let store: Arc<dyn Store> = Arc::new(minio(&fake));
    assert_eq!(store.get("mail", "k").unwrap(), b"x");
}
