//! The fake store's own checks, driven by raw requests: what it refuses before and after the
//! signature, which calls it serves, and its operations' record.

use js_logstore::fake::{FakeStore, Op, Policy};
use js_logstore::{Clock, Credential, HttpRequest, HttpResponse, Method, Transport};

use crate::support::{ALL, Raw, Rig, code, policy};

/// The fake's answer to `request`.
fn answer(fake: &FakeStore, request: HttpRequest) -> HttpResponse {
    fake.send(request).unwrap()
}

/// The status and S3 code of the fake's answer to `request`.
fn refusal(fake: &FakeStore, request: HttpRequest) -> (u16, String) {
    let response = answer(fake, request);
    (response.status, code(&response))
}

/// A rig and a credential that may do everything.
fn rig() -> (Rig, Credential) {
    let rig = Rig::new();
    let credential = rig.fake.issue(policy(&ALL, ""));
    (rig, credential)
}

/// `request` with the header `name` set to `value`.
fn with_header(mut request: HttpRequest, name: &str, value: &str) -> HttpRequest {
    request.headers.retain(|(have, _)| have != name);
    request.headers.push((name.to_owned(), value.to_owned()));
    request
}

/// Asserts that a request whose `Authorization` is missing, not Signature Version 4, or cut
/// short, and one whose `x-amz-date` is not a valid `YYYYMMDDTHHMMSSZ` instant, is refused
/// with `SignatureDoesNotMatch` before anything else is looked at.
#[test]
fn a_request_without_a_readable_signature_is_refused() {
    let (rig, credential) = rig();
    let now = rig.clock.now();
    let request = || {
        Raw::new(Method::Get, "/logs/k")
            .header("range", "bytes=0-1")
            .sign(&credential, now)
    };
    let mismatch = (403, "SignatureDoesNotMatch".to_owned());

    let mut missing = request();
    missing.headers.retain(|(name, _)| name != "authorization");
    assert_eq!(refusal(&rig.fake, missing), mismatch);
    for authorization in [
        "Bearer token",
        "AWS4-HMAC-SHA256 Credential=FAKEKEY000000000001",
        "AWS4-HMAC-SHA256 Credential=FAKEKEY000000000001/20270115/us-east-1/s3/aws4_request",
        "AWS4-HMAC-SHA256 Credential=FAKEKEY000000000001/x, SignedHeaders=host",
    ] {
        let request = with_header(request(), "authorization", authorization);
        assert_eq!(refusal(&rig.fake, request), mismatch, "{authorization}");
    }
    for stamp in [
        "",
        "20270115T080000",
        "20270115X080000Z",
        "20270115T080000X",
        "2027011aT080000Z",
        "2027+115T080000Z",
        "20271315T080000Z",
        "20270132T080000Z",
        "20270115T250000Z",
        "20270115T086000Z",
        "20270115T080060Z",
        "\u{e9}0270115T08000Z",
        "202\u{e9}115T080000Z",
    ] {
        let request = with_header(request(), "x-amz-date", stamp);
        assert_eq!(refusal(&rig.fake, request), mismatch, "{stamp:?}");
    }
    let mut no_date = request();
    no_date.headers.retain(|(name, _)| name != "x-amz-date");
    assert_eq!(refusal(&rig.fake, no_date), mismatch);
    assert!(rig.fake.operations().is_empty());
}

/// Asserts that a signed request whose URL has no scheme, no path or an undecodable query is
/// `InvalidURI`; that one for another host or another bucket, or naming a key that is not
/// valid percent-encoded UTF-8, is `NoSuchBucket`; that a method the fake does not serve on
/// the object or the bucket is `MethodNotAllowed`; and that a read without a valid range and a
/// listing that is not ListObjectsV2 with a `max-keys` of 1 to 1000 are `InvalidRequest`.
#[test]
fn a_call_the_fake_does_not_serve_is_refused() {
    let (rig, credential) = rig();
    let now = rig.clock.now();
    let fake = &rig.fake;
    let signed = |raw: Raw<'_>| raw.sign(&credential, now);

    for url in [
        "store.example.com/logs/k",
        "https://store.example.com",
        "https://store.example.com/logs?a=%zz",
        "https://store.example.com/logs?a=%ff",
    ] {
        let mut request = signed(Raw::new(Method::Delete, "/logs/k"));
        request.url = url.to_owned();
        assert_eq!(
            refusal(fake, request),
            (400, "InvalidURI".to_owned()),
            "{url}"
        );
    }

    let other_host = Raw {
        host: "other.example.com",
        ..Raw::new(Method::Delete, "/logs/k")
    };
    let no_bucket = (404, "NoSuchBucket".to_owned());
    assert_eq!(refusal(fake, signed(other_host)), no_bucket);
    for path in [
        "/other/k",
        "/logsx/k",
        "/logs/%zz",
        "/logs/%4",
        "/logs/a%",
        "/logs/%FF",
    ] {
        assert_eq!(
            refusal(fake, signed(Raw::new(Method::Delete, path))),
            no_bucket,
            "{path}"
        );
    }

    let not_allowed = (405, "MethodNotAllowed".to_owned());
    for (method, path) in [
        (Method::Post, "/logs/k"),
        (Method::Put, "/logs"),
        (Method::Delete, "/logs/"),
        (Method::Post, "/logs"),
    ] {
        assert_eq!(
            refusal(fake, signed(Raw::new(method, path))),
            not_allowed,
            "{method:?} {path}"
        );
    }

    let invalid = (400, "InvalidRequest".to_owned());
    assert_eq!(
        refusal(fake, signed(Raw::new(Method::Get, "/logs/k"))),
        invalid
    );
    for range in [
        "bytes=5-2",
        "bytes=a-9",
        "bytes=0-b",
        "bytes=0",
        "items=0-1",
    ] {
        let raw = Raw::new(Method::Get, "/logs/k").header("range", range);
        let raw = Raw {
            sign_also: &["range"],
            ..raw
        };
        assert_eq!(refusal(fake, signed(raw)), invalid, "{range}");
    }
    let listings = [
        Raw::new(Method::Get, "/logs").query("max-keys", "5"),
        Raw::new(Method::Get, "/logs")
            .query("list-type", "1")
            .query("max-keys", "5"),
        Raw::new(Method::Get, "/logs").query("list-type", "2"),
        Raw::new(Method::Get, "/logs")
            .query("list-type", "2")
            .query("max-keys", "0"),
        Raw::new(Method::Get, "/logs")
            .query("list-type", "2")
            .query("max-keys", "1001"),
        Raw::new(Method::Get, "/logs")
            .query("list-type", "2")
            .query("max-keys", "many"),
    ];
    for raw in listings {
        assert_eq!(refusal(fake, signed(raw)), invalid);
    }
    assert!(fake.operations().is_empty());
}

/// Asserts what the fake serves to raw requests: a listing at `/bucket/` with no prefix, its
/// keys escaped for XML; a ranged read whose end is past the object's; and that a policy is
/// checked on the action and on the key's prefix separately.
#[test]
fn the_fake_serves_raw_requests_as_s3_does() {
    let (rig, credential) = rig();
    let now = rig.clock.now();
    let fake = &rig.fake;
    let admin = rig.client(credential.clone());
    admin
        .put("a&b<c>d", b"0123456789".to_vec(), &Default::default())
        .unwrap();

    let listing = Raw::new(Method::Get, "/logs/")
        .query("list-type", "2")
        .query("max-keys", "1000")
        .sign(&credential, now);
    let body = String::from_utf8(answer(fake, listing).body).unwrap();
    assert!(body.contains("<Key>a&amp;b&lt;c&gt;d</Key>"), "{body}");
    assert!(body.contains("<Prefix></Prefix>"), "{body}");
    assert!(body.contains("<Name>logs</Name>"), "{body}");
    assert!(body.contains("<IsTruncated>false</IsTruncated>"), "{body}");
    assert!(
        body.contains("<LastModified>2027-01-15T08:00:00.000Z</LastModified>"),
        "{body}"
    );

    let range = Raw {
        sign_also: &["range"],
        ..Raw::new(Method::Get, "/logs/a%26b%3Cc%3Ed").header("range", "bytes=8-100")
    };
    let read = answer(fake, range.sign(&credential, now));
    assert_eq!(read.status, 206);
    assert_eq!(read.body, b"89");
    assert_eq!(read.header("content-range"), Some("bytes 8-9/10"));

    let delete_only = rig.client(fake.issue(Policy {
        actions: vec![js_logstore::fake::Action::Delete],
        prefix: "a".to_owned(),
    }));
    assert!(
        delete_only
            .put("a&b<c>d", b"x".to_vec(), &Default::default())
            .is_err()
    );
    assert!(delete_only.delete("b").is_err());
    delete_only.delete("a&b<c>d").unwrap();
    assert_eq!(
        fake.operations(),
        [
            Op::Put {
                key: "a&b<c>d".to_owned(),
                len: 10
            },
            Op::List {
                prefix: String::new()
            },
            Op::Get {
                key: "a&b<c>d".to_owned()
            },
            Op::Delete {
                key: "a&b<c>d".to_owned()
            },
        ]
    );
    assert_eq!(
        format!("{fake:?}"),
        "FakeStore { endpoint: Endpoint { https: true, authority: \"store.example.com\", region: \"us-east-1\", bucket: \"logs\", path_style: true }, objects: 0, credentials: 2, .. }"
    );
}
