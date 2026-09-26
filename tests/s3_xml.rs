//! Parsing S3's XML bodies. The fixtures follow the response shapes in the S3 API reference
//! (ListBuckets, ListObjectsV2 with and without `encoding-type=url`, and the Error document).

use reses::s3::xml::{ErrorDoc, parse_buckets, parse_error, parse_listing};
use reses::s3::{Bucket, ObjectInfo, S3Error};
use time::macros::datetime;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/s3/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn list_all_my_buckets() {
    assert_eq!(
        parse_buckets(&fixture("list_buckets.xml")).unwrap(),
        vec![
            Bucket {
                name: "mail-inbound".into(),
                created: Some(datetime!(2019-12-11 23:32:47 UTC)),
            },
            Bucket {
                name: "quotes.example".into(),
                created: Some(datetime!(2006-02-03 16:41:58 UTC)),
            },
        ]
    );
}

#[test]
fn list_all_my_buckets_empty() {
    assert_eq!(
        parse_buckets(&fixture("list_buckets_empty.xml")).unwrap(),
        vec![]
    );
}

#[test]
fn list_objects_v2_with_folders_and_a_next_page() {
    let l = parse_listing(&fixture("list_objects_v2_folders.xml"), true).unwrap();
    assert_eq!(l.prefixes, vec!["inbox/2024/", "inbox/spam folder/"]);
    assert_eq!(
        l.objects,
        vec![
            ObjectInfo {
                key: "inbox/0a1b2c3d4e5f".into(),
                size: 48213,
                last_modified: Some(datetime!(2024-03-01 09:15:02 UTC)),
            },
            ObjectInfo {
                // `+` is a space and `%2B` a plus in S3's url encoding-type.
                key: "inbox/a b+cé%.eml".into(),
                size: 0,
                last_modified: Some(datetime!(2024-03-02 10:00:00 UTC)),
            },
        ]
    );
    // The token is opaque and never url-encoded, so it comes back byte for byte.
    assert_eq!(
        l.next_token.as_deref(),
        Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
    );
}

#[test]
fn list_objects_v2_last_page_has_no_token() {
    let l = parse_listing(&fixture("list_objects_v2_last_page.xml"), false).unwrap();
    assert_eq!(l.prefixes, Vec::<String>::new());
    assert_eq!(l.objects.len(), 1);
    assert_eq!(l.objects[0].key, "inbox/zz-last");
    assert_eq!(l.objects[0].size, 7);
    assert_eq!(
        l.objects[0].last_modified,
        Some(datetime!(2024-03-03 00:00:00 UTC))
    );
    assert_eq!(l.next_token, None);
}

#[test]
fn list_objects_v2_without_url_encoding_leaves_keys_alone() {
    // A plain listing with a literal `+` in a key must not turn it into a space.
    let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>false</IsTruncated><Contents><Key>a+b%20c</Key><Size>1</Size></Contents></ListBucketResult>"#;
    let l = parse_listing(body, false).unwrap();
    assert_eq!(l.objects[0].key, "a+b%20c");
    assert_eq!(l.objects[0].last_modified, None);
}

#[test]
fn list_objects_v2_empty() {
    let l = parse_listing(&fixture("list_objects_v2_empty.xml"), false).unwrap();
    assert_eq!(l, reses::s3::Listing::default());
}

#[test]
fn error_documents() {
    assert_eq!(
        parse_error(&fixture("error_no_such_key.xml")).unwrap(),
        ErrorDoc {
            code: "NoSuchKey".into(),
            message: "The specified key does not exist.".into(),
            region: None,
            endpoint: None,
        }
    );
    assert_eq!(
        parse_error(&fixture("error_authorization_header_malformed.xml"))
            .unwrap()
            .region
            .as_deref(),
        Some("eu-west-2")
    );
    let redirect = parse_error(&fixture("error_permanent_redirect.xml")).unwrap();
    assert_eq!(redirect.code, "PermanentRedirect");
    assert_eq!(
        redirect.endpoint.as_deref(),
        Some("mail-inbound.s3.eu-west-2.amazonaws.com")
    );
    assert_eq!(
        parse_error(&fixture("error_access_denied.xml"))
            .unwrap()
            .code,
        "AccessDenied"
    );
}

#[test]
fn malformed_xml_is_a_parse_error() {
    for name in ["malformed_truncated.xml", "malformed_bad_size.xml"] {
        let err = parse_listing(&fixture(name), false).unwrap_err();
        assert!(matches!(err, S3Error::Parse(_)), "{name}: {err:?}");
    }
    assert!(matches!(
        parse_buckets(b"<ListAllMyBucketsResult><Buckets><Bucket>"),
        Err(S3Error::Parse(_))
    ));
    assert!(matches!(parse_error(b"not xml at all <"), Err(S3Error::Parse(_))));
    // The wrong document entirely is not a listing.
    assert!(matches!(
        parse_listing(&fixture("error_no_such_key.xml"), false),
        Err(S3Error::Parse(_))
    ));
}
