//! How the client reads the store's answers, from scripted replies: the error each status and
//! code is, the checks on a ranged answer, the list scanner, and the requests it sends.

use js_logstore::{Metadata, Method, StoreError, TransportError, Unavailable};

use crate::support::{Scripted, reply, s3_error, scripted_client};
use crate::vectors::S3_LIST_ROOT;

/// A listing answer: `truncated` and one `Contents` per `(key, size, last_modified)`.
fn page(truncated: &str, contents: &[(&str, &str, &str)]) -> Vec<u8> {
    let contents: String = contents
        .iter()
        .map(|(key, size, at)| {
            format!(
                "<Contents><Key>{key}</Key><LastModified>{at}</LastModified><ETag>&quot;e&quot;</ETag><Size>{size}</Size></Contents>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\"?><ListBucketResult><Name>logs</Name><IsTruncated>{truncated}</IsTruncated><Unknown>ignored</Unknown>{contents}</ListBucketResult>"
    )
    .into_bytes()
}

/// A valid `LastModified`.
const AT: &str = "2027-01-15T08:00:00.000Z";

/// Asserts the error each refusal is: an expiry code `Expired` whatever its status; `SlowDown`
/// with any status, a 429 and every 5xx `Unavailable`; another 403 `Denied`; a 404 `NotFound`;
/// any other status `Malformed`; a code that is too long or not letters and digits kept as
/// empty; a lost exchange `Unavailable` and an answer over the transport's cap `Malformed`.
#[test]
fn each_refusal_is_the_error_its_status_and_code_say() {
    let unavailable = |status: u16, code: &str| {
        StoreError::Unavailable(Unavailable::Status {
            status,
            code: code.to_owned(),
        })
    };
    let malformed = StoreError::Malformed("the store answered a status the client does not expect");
    let long_code = "C".repeat(65);
    let longest_code = "C".repeat(64);
    let cases = [
        (
            s3_error(400, "ExpiredToken"),
            StoreError::Expired {
                code: "ExpiredToken".to_owned(),
            },
        ),
        (
            s3_error(403, "RequestExpired"),
            StoreError::Expired {
                code: "RequestExpired".to_owned(),
            },
        ),
        (
            s3_error(400, "InvalidToken"),
            StoreError::Expired {
                code: "InvalidToken".to_owned(),
            },
        ),
        (s3_error(400, "SlowDown"), unavailable(400, "SlowDown")),
        (s3_error(503, "SlowDown"), unavailable(503, "SlowDown")),
        (s3_error(429, ""), unavailable(429, "")),
        (
            s3_error(500, "InternalError"),
            unavailable(500, "InternalError"),
        ),
        (reply(599, &[], b""), unavailable(599, "")),
        (
            s3_error(403, "AccessDenied"),
            StoreError::Denied {
                status: 403,
                code: "AccessDenied".to_owned(),
            },
        ),
        (
            s3_error(403, &long_code),
            StoreError::Denied {
                status: 403,
                code: String::new(),
            },
        ),
        (
            s3_error(403, &longest_code),
            StoreError::Denied {
                status: 403,
                code: longest_code.clone(),
            },
        ),
        (
            s3_error(403, "Bad-Code"),
            StoreError::Denied {
                status: 403,
                code: String::new(),
            },
        ),
        (
            reply(403, &[], b"<Code>Unclosed"),
            StoreError::Denied {
                status: 403,
                code: String::new(),
            },
        ),
        (
            s3_error(404, "NoSuchBucket"),
            StoreError::NotFound {
                code: "NoSuchBucket".to_owned(),
            },
        ),
        (s3_error(400, "InvalidArgument"), malformed.clone()),
        (
            reply(301, &[("location", "https://elsewhere.example.com/")], b""),
            malformed.clone(),
        ),
        (reply(100, &[], b""), malformed.clone()),
        (
            Err(TransportError::Timeout),
            StoreError::Unavailable(Unavailable::Transport(TransportError::Timeout)),
        ),
        (
            Err(TransportError::Exchange),
            StoreError::Unavailable(Unavailable::Transport(TransportError::Exchange)),
        ),
        (
            Err(TransportError::TooLarge),
            StoreError::Malformed("the answer is longer than is read"),
        ),
    ];
    for (answer, expected) in cases {
        let transport = Scripted::new(vec![answer]);
        let store = scripted_client(transport);
        assert_eq!(
            store.put("k", b"x".to_vec(), &Metadata::new()),
            Err(expected)
        );
    }
}

/// Asserts how delete, get and list read their statuses: a delete is done on any 2xx and on
/// 404; a get is the range on 206, `None` on 404, `OutOfRange` on 416, and a refusal otherwise
/// (a 200 included: the client asked for a range); a list is parsed on 200 only.
#[test]
fn each_call_reads_its_statuses() {
    let transport = Scripted::new(vec![
        reply(204, &[], b""),
        reply(200, &[], b""),
        s3_error(404, "NoSuchKey"),
        s3_error(404, "NoSuchBucket"),
        s3_error(500, "InternalError"),
        reply(404, &[], b""),
        s3_error(404, "NoSuchKey"),
        s3_error(404, "NoSuchBucket"),
        reply(416, &[], b""),
        reply(200, &[("content-range", "bytes 0-1/2")], b"ab"),
        s3_error(403, "AccessDenied"),
        s3_error(404, "NoSuchBucket"),
        reply(201, &[], &page("false", &[])),
    ]);
    let store = scripted_client(transport);
    let no_bucket = StoreError::NotFound {
        code: "NoSuchBucket".to_owned(),
    };
    assert_eq!(store.delete("k"), Ok(()));
    assert_eq!(store.delete("k"), Ok(()));
    assert_eq!(store.delete("k"), Ok(()));
    assert_eq!(store.delete("k"), Ok(()));
    assert_eq!(store.delete("k"), Err(no_bucket.clone()));
    assert!(matches!(store.delete("k"), Err(StoreError::Unavailable(_))));
    assert_eq!(store.get_range("k", 0, 2), Ok(None));
    assert_eq!(store.get_range("k", 0, 2), Ok(None));
    assert_eq!(store.get_range("k", 0, 2), Err(no_bucket));
    assert_eq!(store.get_range("k", 0, 2), Err(StoreError::OutOfRange));
    assert!(matches!(
        store.get_range("k", 0, 2),
        Err(StoreError::Malformed(_))
    ));
    assert!(matches!(
        store.list("", None, 1),
        Err(StoreError::Denied { .. })
    ));
    assert!(matches!(
        store.list("", None, 1),
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.list("", None, 1),
        Err(StoreError::Malformed(_))
    ));
}

/// The headers and body of a scripted ranged answer.
type Case<'a> = (&'a [(&'a str, &'a str)], &'a [u8]);

/// Asserts that a 206 is read only when its `content-range` is exactly the range asked for
/// and as long as its body: a missing or unparsable header, another first byte, a last byte
/// before the first, a body of another length or a last byte past the total is `Malformed`;
/// a valid one returns its bytes, its total and its `x-amz-meta-*` headers only.
#[test]
fn a_ranged_answer_must_be_the_range_asked_for() {
    let refused: [Case; 11] = [
        (&[("content-range", "bytes 5/10")], b"ab"),
        (&[("content-range", "bytes 0-x/10")], b"ab"),
        (&[], b"ab"),
        (&[("content-range", "0-1/10")], b"ab"),
        (&[("content-range", "bytes 0-1")], b"ab"),
        (&[("content-range", "bytes x-1/10")], b"ab"),
        (&[("content-range", "bytes 5-6/10")], b"ab"),
        (&[("content-range", "bytes 0-1/*")], b"ab"),
        (&[("content-range", "bytes 0-3/10")], b"ab"),
        (&[("content-range", "bytes 0-1/1")], b"ab"),
        (
            &[(
                "content-range",
                "bytes 0-18446744073709551615/18446744073709551615",
            )],
            b"ab",
        ),
    ];
    for (headers, body) in refused {
        let store = scripted_client(Scripted::new(vec![reply(206, headers, body)]));
        assert!(
            matches!(store.get_range("k", 0, 2), Err(StoreError::Malformed(_))),
            "{headers:?}"
        );
    }
    let backwards = scripted_client(Scripted::new(vec![reply(
        206,
        &[("content-range", "bytes 5-4/10")],
        b"",
    )]));
    assert!(matches!(
        backwards.get_range("k", 5, 2),
        Err(StoreError::Malformed(_))
    ));

    let headers = [
        ("content-range", "bytes 4-5/6"),
        ("x-amz-meta-first-byte-at-ms", "12"),
        ("x-amz-request-id", "r"),
        ("content-type", "application/octet-stream"),
    ];
    let store = scripted_client(Scripted::new(vec![reply(206, &headers, b"ef")]));
    let object = store.get_range("k", 4, 10).unwrap().unwrap();
    assert_eq!(object.bytes, b"ef");
    assert_eq!(object.total_len, 6);
    assert_eq!(
        object.metadata,
        Metadata::from([("first-byte-at-ms".to_owned(), "12".to_owned())])
    );
}

/// Asserts the list scanner: elements it does not use are ignored; XML references in a key are
/// read; and an answer that is not UTF-8, has no `ListBucketResult`, no `IsTruncated` of
/// `true` or `false`, an unclosed `Contents`, a `Key`, `Size` or `LastModified` missing or
/// malformed, a key not after the one before it (or `start_after`), a key outside the prefix,
/// more keys than asked, or is truncated with no key, is `Malformed`.
#[test]
fn the_list_scanner_reads_only_a_well_formed_page() {
    let good = page(
        "true",
        &[
            ("p/&amp;&lt;&gt;&quot;&apos;&#65;&#x42;", "0", AT),
            ("p/b", "18446744073709551615", AT),
        ],
    );
    let store = scripted_client(Scripted::new(vec![reply(200, &[], &good)]));
    let listed = store.list("p/", Some("p/!"), 2).unwrap();
    assert!(listed.truncated);
    let keys: Vec<&str> = listed
        .entries
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(keys, ["p/&<>\"'AB", "p/b"]);
    assert_eq!(listed.entries[1].size, u64::MAX);

    let with_namespace = format!(
        "{S3_LIST_ROOT}<IsTruncated>false</IsTruncated><Contents><Key>p/a</Key><Size>1</Size><LastModified>{AT}</LastModified></Contents></ListBucketResult>"
    );
    let store = scripted_client(Scripted::new(vec![reply(
        200,
        &[],
        with_namespace.as_bytes(),
    )]));
    assert_eq!(store.list("p/", None, 1).unwrap().entries[0].key, "p/a");

    let empty = scripted_client(Scripted::new(vec![reply(200, &[], &page("false", &[]))]));
    assert_eq!(empty.list("p/", None, 1).unwrap().entries.len(), 0);

    let refused: Vec<(Vec<u8>, Option<&str>, u32)> = vec![
        (b"\xff".to_vec(), None, 10),
        (b"<Other></Other>".to_vec(), None, 10),
        (b"<ListBucketResult></ListBucketResult>".to_vec(), None, 10),
        (b"<ListBucketResultX><IsTruncated>false</IsTruncated></ListBucketResultX>".to_vec(), None, 10),
        (b"<ListBucketResult a=\"b\"/><IsTruncated>false</IsTruncated></ListBucketResult>".to_vec(), None, 10),
        (b"<ListBucketResult".to_vec(), None, 10),
        (b"<ListBucketResult><IsTruncated>false</IsTruncated>".to_vec(), None, 10),
        (page("maybe", &[]), None, 10),
        (
            b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>p/a</Key></ListBucketResult>".to_vec(),
            None,
            10,
        ),
        (
            format!(
                "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Size>1</Size><LastModified>{AT}</LastModified></Contents></ListBucketResult>"
            )
            .into_bytes(),
            None,
            10,
        ),
        (page("false", &[("p/a<b", "1", AT)]), None, 10),
        (page("false", &[("p/&bogus;", "1", AT)]), None, 10),
        (page("false", &[("p/&amp", "1", AT)]), None, 10),
        (page("false", &[("p/&#xD800;", "1", AT)]), None, 10),
        (page("false", &[("p/&#xZZ;", "1", AT)]), None, 10),
        (page("false", &[("p/&#99999999999;", "1", AT)]), None, 10),
        (page("false", &[("p/a", "", AT)]), None, 10),
        (page("false", &[("p/a", "1a", AT)]), None, 10),
        (page("false", &[("p/a", "+1", AT)]), None, 10),
        (page("false", &[("p/a", "99999999999999999999", AT)]), None, 10),
        (page("false", &[("p/a", "1", "yesterday")]), None, 10),
        (page("false", &[("p/b", "1", AT), ("p/a", "1", AT)]), None, 10),
        (page("false", &[("p/a", "1", AT), ("p/a", "1", AT)]), None, 10),
        (page("false", &[("p/a", "1", AT)]), Some("p/a"), 10),
        (page("false", &[("p/a", "1", AT)]), Some("p/b"), 10),
        (page("false", &[("q/a", "1", AT)]), None, 10),
        (page("false", &[("p/a", "1", AT), ("p/b", "1", AT)]), None, 1),
        (page("true", &[]), None, 10),
    ];
    for (body, start_after, max_keys) in refused {
        let store = scripted_client(Scripted::new(vec![reply(200, &[], &body)]));
        let listed = store.list("p/", start_after, max_keys);
        assert!(
            matches!(listed, Err(StoreError::Malformed(_))),
            "{}: {listed:?}",
            String::from_utf8_lossy(&body)
        );
    }
}

/// Asserts the requests the client sends: the method; the path-style URL with the S3-encoded
/// key; a listing's ListObjectsV2 query, with `start-after` only when given; the host and the
/// signer's headers; and a ranged read's `range`, signed.
#[test]
fn the_requests_are_the_documented_ones() {
    let transport = Scripted::new(vec![
        reply(200, &[], b""),
        reply(204, &[], b""),
        reply(404, &[], b""),
        reply(200, &[], &page("false", &[])),
        reply(200, &[], &page("false", &[])),
    ]);
    let store = scripted_client(transport.clone());
    store
        .put("a b/c", b"body".to_vec(), &Metadata::new())
        .unwrap();
    store.delete("a b/c").unwrap();
    assert_eq!(store.get_range("a b/c", 7, 3), Ok(None));
    store.list("js/x y/", None, 1000).unwrap();
    store.list("js/", Some("js/a/b"), 5).unwrap();

    let sent = transport.sent();
    let methods: Vec<Method> = sent.iter().map(|request| request.method).collect();
    assert_eq!(
        methods,
        [
            Method::Put,
            Method::Delete,
            Method::Get,
            Method::Get,
            Method::Get
        ]
    );
    let urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://store.example.com/logs/a%20b/c",
            "https://store.example.com/logs/a%20b/c",
            "https://store.example.com/logs/a%20b/c",
            "https://store.example.com/logs?list-type=2&max-keys=1000&prefix=js%2Fx%20y%2F",
            "https://store.example.com/logs?list-type=2&max-keys=5&prefix=js%2F&start-after=js%2Fa%2Fb",
        ]
    );
    let names: Vec<&str> = sent[2]
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "range",
            "host",
            "x-amz-date",
            "x-amz-content-sha256",
            "x-amz-security-token",
            "authorization"
        ]
    );
    assert_eq!(sent[2].header("range"), Some("bytes=7-9"));
    assert_eq!(sent[2].header("host"), Some("store.example.com"));
    assert_eq!(sent[2].header("x-amz-date"), Some("20270115T080000Z"));
    assert_eq!(sent[2].header("x-amz-security-token"), Some("token"));
    assert!(sent[2].header("authorization").unwrap().starts_with(
        "AWS4-HMAC-SHA256 Credential=AKIDTEST/20270115/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature="
    ));
    assert_eq!(sent[0].body, b"body");
}
