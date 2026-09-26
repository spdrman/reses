//! The SigV4 signer, pinned to the worked examples on AWS's "Signature Calculations for the
//! Authorization Header" page for S3, plus a session-token case whose expected values I
//! computed with a separate Python hmac/hashlib script from the canonical request below.

use reses::s3::Credentials;
use reses::s3::sigv4::{self, EMPTY_SHA256, Request, canonical_query, uri_encode};
use time::OffsetDateTime;
use time::macros::datetime;

const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEXAMPLE/session+token==";
const WHEN: OffsetDateTime = datetime!(2013-05-24 0:00 UTC);

fn creds(token: Option<&str>) -> Credentials {
    Credentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_access_key: SECRET.into(),
        session_token: token.map(str::to_string),
    }
}

fn get_object(token: Option<&str>) -> sigv4::Signature {
    sigv4::sign(
        &Request {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            query: &[],
            headers: &[("Range", "bytes=0-9")],
            payload_sha256: EMPTY_SHA256,
        },
        &creds(token),
        "us-east-1",
        "s3",
        WHEN,
    )
}

#[test]
fn empty_payload_hash_is_sha256_of_nothing() {
    assert_eq!(sigv4::sha256_hex(b""), EMPTY_SHA256);
    assert_eq!(
        sigv4::sha256_hex(b"Welcome to Amazon S3."),
        "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
    );
}

#[test]
fn get_object_example_matches_aws_docs() {
    let s = get_object(None);
    assert_eq!(
        s.canonical_request,
        "GET\n\
         /test.txt\n\
         \n\
         host:examplebucket.s3.amazonaws.com\n\
         range:bytes=0-9\n\
         x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
         x-amz-date:20130524T000000Z\n\
         \n\
         host;range;x-amz-content-sha256;x-amz-date\n\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        s.string_to_sign,
        "AWS4-HMAC-SHA256\n\
         20130524T000000Z\n\
         20130524/us-east-1/s3/aws4_request\n\
         7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
    );
    assert_eq!(
        s.signature,
        "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
    assert_eq!(
        s.authorization,
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
         SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
         Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
}

#[test]
fn signed_headers_to_send_include_date_hash_and_authorization() {
    let s = get_object(None);
    let names: Vec<&str> = s.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["x-amz-date", "x-amz-content-sha256", "authorization"]);
    assert_eq!(s.headers[0].1, "20130524T000000Z");
    assert_eq!(s.headers[1].1, EMPTY_SHA256);
    assert_eq!(s.headers[2].1, s.authorization);
}

#[test]
fn list_objects_example_matches_aws_docs() {
    let s = sigv4::sign(
        &Request {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/",
            // Out of order on purpose: the signer sorts.
            query: &[("prefix", "J"), ("max-keys", "2")],
            headers: &[],
            payload_sha256: EMPTY_SHA256,
        },
        &creds(None),
        "us-east-1",
        "s3",
        WHEN,
    );
    assert_eq!(
        s.canonical_request,
        "GET\n\
         /\n\
         max-keys=2&prefix=J\n\
         host:examplebucket.s3.amazonaws.com\n\
         x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
         x-amz-date:20130524T000000Z\n\
         \n\
         host;x-amz-content-sha256;x-amz-date\n\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        s.string_to_sign,
        "AWS4-HMAC-SHA256\n\
         20130524T000000Z\n\
         20130524/us-east-1/s3/aws4_request\n\
         df57d21db20da04d7fa30298dd4488ba3a2b47ca3a489c74750e0f1e7df1b9b7"
    );
    assert_eq!(
        s.signature,
        "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
    );
}

#[test]
fn session_token_is_signed_and_sent() {
    let s = get_object(Some(TOKEN));
    assert_eq!(
        s.canonical_request,
        format!(
            "GET\n/test.txt\n\n\
             host:examplebucket.s3.amazonaws.com\n\
             range:bytes=0-9\n\
             x-amz-content-sha256:{EMPTY_SHA256}\n\
             x-amz-date:20130524T000000Z\n\
             x-amz-security-token:{TOKEN}\n\
             \n\
             host;range;x-amz-content-sha256;x-amz-date;x-amz-security-token\n\
             {EMPTY_SHA256}"
        )
    );
    assert_eq!(
        s.string_to_sign,
        "AWS4-HMAC-SHA256\n\
         20130524T000000Z\n\
         20130524/us-east-1/s3/aws4_request\n\
         a66e2fc9e376bde8d20c255cc53d06e1c59445ab1321990d64dc8f073b4550bc"
    );
    assert_eq!(
        s.signature,
        "d701df8c70d285aeab447130786fd68bde8ee78a503297c967264666295eb479"
    );
    assert!(
        s.headers
            .iter()
            .any(|(k, v)| k == "x-amz-security-token" && v == TOKEN)
    );
}

#[test]
fn signature_debug_hides_the_token_and_secret() {
    let s = get_object(Some(TOKEN));
    let dbg = format!("{s:?}");
    assert!(!dbg.contains(TOKEN), "{dbg}");
    assert!(!dbg.contains(SECRET), "{dbg}");
}

#[test]
fn a_different_region_changes_the_scope() {
    let s = sigv4::sign(
        &Request {
            method: "GET",
            host: "examplebucket.s3.eu-west-2.amazonaws.com",
            path: "/test.txt",
            query: &[],
            headers: &[],
            payload_sha256: EMPTY_SHA256,
        },
        &creds(None),
        "eu-west-2",
        "s3",
        WHEN,
    );
    assert!(
        s.string_to_sign
            .contains("\n20130524/eu-west-2/s3/aws4_request\n")
    );
    assert!(
        s.authorization
            .contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/eu-west-2/s3/aws4_request,")
    );
}

#[test]
fn header_values_are_trimmed_and_names_lowercased() {
    let s = sigv4::sign(
        &Request {
            method: "GET",
            host: "h",
            path: "/k",
            query: &[],
            headers: &[("X-Amz-Meta-Thing", "  a   b  ")],
            payload_sha256: EMPTY_SHA256,
        },
        &creds(None),
        "us-east-1",
        "s3",
        WHEN,
    );
    assert!(s.canonical_request.contains("\nx-amz-meta-thing:a b\n"));
    assert!(
        s.canonical_request
            .contains("\nhost;x-amz-content-sha256;x-amz-date;x-amz-meta-thing\n")
    );
}

#[test]
fn uri_encode_follows_s3_rules() {
    assert_eq!(uri_encode("AZaz09-_.~", false), "AZaz09-_.~");
    assert_eq!(
        uri_encode("inbox/a b+c/é%.eml", true),
        "inbox/a%20b%2Bc/%C3%A9%25.eml"
    );
    assert_eq!(uri_encode("a/b", false), "a%2Fb");
    assert_eq!(uri_encode("=&?#*!'()", true), "%3D%26%3F%23%2A%21%27%28%29");
    assert_eq!(uri_encode("", true), "");
}

#[test]
fn canonical_query_encodes_and_sorts() {
    assert_eq!(
        canonical_query(&[
            ("prefix", "a b/c"),
            ("list-type", "2"),
            ("delimiter", "/"),
            ("continuation-token", "1+x="),
            ("encoding-type", "url"),
        ]),
        "continuation-token=1%2Bx%3D&delimiter=%2F&encoding-type=url&list-type=2&prefix=a%20b%2Fc"
    );
    assert_eq!(canonical_query(&[("uploads", "")]), "uploads=");
    assert_eq!(canonical_query(&[]), "");
}
