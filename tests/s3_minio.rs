//! `S3Client` against a real MinIO. These are `#[ignore]` so the normal gate stays offline;
//! `scripts/ci-docker.sh --integration` starts MinIO and runs them with the endpoint and its
//! throwaway credentials in the environment.

use std::collections::BTreeSet;

use reses::s3::{Credentials, S3Client, S3Error, Store};

/// A variable ci-docker.sh sets for the integration run. Missing means someone ran these by hand,
/// so the panic says where to run them from.
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set; run these through scripts/ci-docker.sh --integration")
    })
}

/// A client on MinIO's test keys, pointed at its endpoint with path-style addressing.
fn client(region: &str) -> S3Client {
    S3Client::new(
        Credentials {
            access_key_id: env("RESES_TEST_S3_ACCESS_KEY"),
            secret_access_key: env("RESES_TEST_S3_SECRET_KEY"),
            session_token: None,
        },
        region,
    )
    .with_endpoint(&env("RESES_TEST_S3_ENDPOINT"), true)
}

/// A fresh bucket per test, so reruns and test order never matter.
fn bucket(tag: &str) -> String {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    format!("reses-it-{tag}-{nanos}")
}

const KEYS: &[&str] = &[
    "inbox/0001",
    "inbox/0002",
    "inbox/a b+c/é%.eml",
    "inbox/plus+sign",
    "inbox/percent%41",
    "inbox/space here",
    "inbox/sub/nested-1",
    "inbox/sub/nested-2",
    "inbox/ünïcödé ✉.eml",
    "other/elsewhere",
];

/// A fresh bucket holding every key in KEYS, each with a body that names its key.
fn seeded(tag: &str) -> (S3Client, String) {
    let c = client("us-east-1");
    let b = bucket(tag);
    c.create_bucket(&b).unwrap();
    for k in KEYS {
        c.put_object(&b, k, format!("body of {k}").as_bytes())
            .unwrap();
    }
    (c, b)
}

/// ListBuckets includes a bucket I just made.
#[test]
#[ignore]
fn list_buckets_sees_a_new_bucket() {
    let (c, b) = seeded("buckets");
    let names: Vec<String> = c
        .list_buckets()
        .unwrap()
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert!(names.contains(&b), "{names:?}");
}

/// Two keys a page forces a delimited listing over several pages. Every page stays in the
/// limit, the tokens eventually run out, and the union is exactly one level of the folder.
#[test]
#[ignore]
fn page_through_a_folder_listing() {
    let (c, b) = seeded("paging");
    let c = c.with_max_keys(2);
    let mut objects = BTreeSet::new();
    let mut prefixes = BTreeSet::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = c.list(&b, "inbox/", Some("/"), token.as_deref()).unwrap();
        pages += 1;
        assert!(page.objects.len() + page.prefixes.len() <= 2, "{page:?}");
        for o in &page.objects {
            assert_eq!(o.size, format!("body of {}", o.key).len() as u64);
            assert!(o.last_modified.is_some());
            objects.insert(o.key.clone());
        }
        prefixes.extend(page.prefixes);
        token = page.next_token;
        if token.is_none() {
            break;
        }
        assert!(pages < 20, "listing never ended");
    }
    assert!(pages >= 4, "only {pages} pages");
    // Only this level's objects come back, and each sub-folder as a single prefix.
    let expected: BTreeSet<String> = KEYS
        .iter()
        .filter(|k| k.starts_with("inbox/") && !k.starts_with("inbox/sub/"))
        .map(|k| k.to_string())
        .filter(|k| !k.starts_with("inbox/a b+c/"))
        .collect();
    assert_eq!(objects, expected);
    assert_eq!(
        prefixes,
        BTreeSet::from(["inbox/a b+c/".to_string(), "inbox/sub/".to_string()])
    );

    // Without a delimiter everything under the prefix comes back flat.
    let flat = client("us-east-1")
        .list(&b, "inbox/sub/", None, None)
        .unwrap();
    assert_eq!(
        flat.objects
            .iter()
            .map(|o| o.key.as_str())
            .collect::<Vec<_>>(),
        ["inbox/sub/nested-1", "inbox/sub/nested-2"]
    );
    assert!(flat.prefixes.is_empty());
}

/// Keys with spaces, `+`, `%` and non-ASCII survive signing and URL encoding on every call.
#[test]
#[ignore]
fn odd_keys_round_trip_through_list_get_and_delete() {
    let (c, b) = seeded("keys");
    for k in KEYS {
        let listed = c.list(&b, k, None, None).unwrap();
        assert!(
            listed.objects.iter().any(|o| o.key == *k),
            "{k} not listed: {listed:?}"
        );
        assert_eq!(c.get(&b, k).unwrap(), format!("body of {k}").as_bytes());
        c.delete(&b, k).unwrap();
        assert!(c.get(&b, k).unwrap_err().is_not_found(), "{k} still there");
    }
    let rest = c.list(&b, "", None, None).unwrap();
    assert!(rest.objects.is_empty(), "{rest:?}");
}

/// Ranged reads clamp to the object's end (and past it give nothing), and a whole get of a
/// few MB comes back intact.
#[test]
#[ignore]
fn ranged_get_and_get() {
    let c = client("us-east-1");
    let b = bucket("range");
    c.create_bucket(&b).unwrap();
    c.put_object(&b, "msg", b"0123456789").unwrap();
    c.put_object(&b, "empty", b"").unwrap();

    assert_eq!(c.get_range(&b, "msg", 0, 3).unwrap(), b"0123");
    assert_eq!(c.get_range(&b, "msg", 7, 1000).unwrap(), b"789");
    assert_eq!(c.get_range(&b, "msg", 10, 20).unwrap(), b"");
    assert_eq!(c.get_range(&b, "msg", 500, 600).unwrap(), b"");
    assert_eq!(c.get_range(&b, "empty", 0, 99).unwrap(), b"");
    assert_eq!(c.get(&b, "msg").unwrap(), b"0123456789");
    assert_eq!(c.get(&b, "empty").unwrap(), b"");

    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    c.put_object(&b, "big", &big).unwrap();
    assert_eq!(c.get(&b, "big").unwrap(), big);
    assert_eq!(
        c.get_range(&b, "big", 2_999_990, 3_000_100).unwrap(),
        &big[2_999_990..]
    );
}

/// A missing bucket or key comes back as S3's own 404 code, not a generic failure.
#[test]
#[ignore]
fn missing_things_are_service_errors() {
    let c = client("us-east-1");
    match c.get("reses-it-no-such-bucket", "k").unwrap_err() {
        S3Error::Service {
            status: 404, code, ..
        } => assert_eq!(code, "NoSuchBucket"),
        other => panic!("{other:?}"),
    }
    let b = bucket("missing");
    c.create_bucket(&b).unwrap();
    match c.get(&b, "nope").unwrap_err() {
        S3Error::Service {
            status: 404, code, ..
        } => assert_eq!(code, "NoSuchKey"),
        other => panic!("{other:?}"),
    }
    // DeleteObject on a missing key succeeds, as S3 does.
    c.delete(&b, "nope").unwrap();
}

/// A bad secret is a 403 from the server, and neither the message nor Debug holds the secret.
#[test]
#[ignore]
fn a_wrong_secret_is_a_service_error_that_does_not_leak_it() {
    let secret = "definitely-not-the-minio-secret";
    let c = S3Client::new(
        Credentials {
            access_key_id: env("RESES_TEST_S3_ACCESS_KEY"),
            secret_access_key: secret.into(),
            session_token: None,
        },
        "us-east-1",
    )
    .with_endpoint(&env("RESES_TEST_S3_ENDPOINT"), true);
    let err = c.list_buckets().unwrap_err();
    assert!(
        matches!(err, S3Error::Service { status: 403, .. }),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains(secret));
}

/// Nothing listening is a transport error, kept apart from anything the server said.
#[test]
#[ignore]
fn unreachable_endpoint_is_a_transport_error() {
    let c = S3Client::new(
        Credentials {
            access_key_id: "x".into(),
            secret_access_key: "y".into(),
            session_token: None,
        },
        "us-east-1",
    )
    .with_endpoint("http://127.0.0.1:9", true);
    assert!(matches!(c.list_buckets(), Err(S3Error::Transport(_))));
}

/// Review item 26: keys with `.` and `..` in them. The client sends every key as the literal
/// bytes it was given (dots are unreserved, so they go out as `.`), and nothing between it
/// and the server may normalise a path onto a neighbouring key.
///
/// MinIO refuses a key whose segments include a bare `.` or `..` outright, with
/// 400 XMinioInvalidResourceName, for put, get, delete and as a list prefix. Real S3 accepts
/// such keys as opaque strings. Dots that aren't whole segments are ordinary characters and
/// round-trip. The decoys prove no request landed on a normalised path.
#[test]
#[ignore]
fn dot_segment_keys_are_never_normalised() {
    let c = client("us-east-1");
    let b = bucket("dots");
    c.create_bucket(&b).unwrap();

    // Where a normalising client or server would land. None of these is a prefix "folder"
    // of another key: MinIO can't list an object `x` alongside objects under `x/`.
    let decoys = ["inbox/a", "b", "e", "f", "h"];
    for k in decoys {
        c.put_object(&b, k, format!("decoy {k}").as_bytes())
            .unwrap();
    }

    let plain = [
        "inbox/.hidden",
        "inbox/..double",
        "inbox/x.y/..z.",
        "inbox/...",
    ];
    // Dots inside a segment are ordinary characters and have to work on every call.
    for k in plain {
        let body = format!("body of {k}");
        c.put_object(&b, k, body.as_bytes()).unwrap();
        let listed = c.list(&b, k, None, None).unwrap();
        assert!(listed.objects.iter().any(|o| o.key == k), "{k}: {listed:?}");
        assert_eq!(c.get(&b, k).unwrap(), body.as_bytes(), "{k}");
        assert_eq!(c.get_range(&b, k, 0, 3).unwrap(), b"body", "{k}");
        c.delete(&b, k).unwrap();
        assert!(c.get(&b, k).unwrap_err().is_not_found(), "{k}");
    }

    let segments = [
        "inbox/./a",
        "inbox/../b",
        "inbox/c/.",
        "inbox/d/..",
        "./e",
        "../f",
        ".",
        "..",
        "inbox/g/../../h",
    ];
    /// MinIO's refusal of a dot segment.
    fn refused<T>(r: &Result<T, S3Error>) -> bool {
        matches!(r, Err(S3Error::Service { status: 400, code, .. })
            if code == "XMinioInvalidResourceName")
    }
    // Whole dot segments are refused on every call, never quietly resolved.
    for k in segments {
        let body = format!("body of {k}");
        let put = c.put_object(&b, k, body.as_bytes());
        assert!(refused(&put), "{k}: put {put:?}");
        let got = c.get(&b, k);
        assert!(refused(&got), "{k}: get {got:?}");
        let listed = c.list(&b, k, None, None);
        assert!(refused(&listed), "{k}: list {listed:?}");
        let deleted = c.delete(&b, k);
        assert!(refused(&deleted), "{k}: delete {deleted:?}");
    }

    // And not one request landed on a decoy.
    for k in decoys {
        assert_eq!(
            c.get(&b, k).unwrap(),
            format!("decoy {k}").as_bytes(),
            "{k} was touched"
        );
    }
}

/// The profile route end to end: aws-config reads the keys from a credentials file by profile
/// name, and the SDK signs with them against MinIO.
#[test]
#[ignore]
fn a_profile_from_files_works_against_minio() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::write(&config, "[profile minio]\nregion = us-east-1\n").unwrap();
    let credentials = dir.path().join("credentials");
    std::fs::write(
        &credentials,
        format!(
            "[minio]\naws_access_key_id = {}\naws_secret_access_key = {}\n",
            env("RESES_TEST_S3_ACCESS_KEY"),
            env("RESES_TEST_S3_SECRET_KEY")
        ),
    )
    .unwrap();
    let c = S3Client::from_profile_files("minio", None, &config, &credentials)
        .with_endpoint(&env("RESES_TEST_S3_ENDPOINT"), true);
    let b = bucket("profile");
    c.create_bucket(&b).unwrap();
    c.put_object(&b, "inbox/m", b"From: a@example.com\r\n\r\nhi")
        .unwrap();
    assert_eq!(c.get_range(&b, "inbox/m", 0, 3).unwrap(), b"From");
    let names: Vec<String> = c
        .list_buckets()
        .unwrap()
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert!(names.contains(&b), "{names:?}");
}
