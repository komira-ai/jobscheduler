//! Endpoints, the URLs built on them (the store's base URL among them), credentials' expiry, and the clocks.

use std::sync::Arc;
use std::time::Duration;

use js_logstore::{Clock, Endpoint, EndpointError, ManualClock, SystemClock};

use crate::support::{Rig, at, credential, endpoint};

/// Asserts the path-style and virtual-host-style URLs of a bucket and of a key holding
/// characters that need encoding: the S3 encoding, `/` kept in the key, the bucket in the path
/// or in the host; and that both forms reach the same object in the fake store.
#[test]
fn the_url_forms_are_right() {
    let key = "js/build 42/a+b=c&d%e~f.\u{e9}*";
    let encoded = "js/build%2042/a%2Bb%3Dc%26d%25e~f.%C3%A9%2A";

    let path = endpoint(true);
    assert_eq!(path.host(), "store.example.com");
    assert_eq!(path.bucket_path(), "/logs");
    assert_eq!(path.object_path(key), format!("/logs/{encoded}"));
    assert_eq!(
        path.url(&path.object_path(key)),
        format!("https://store.example.com/logs/{encoded}")
    );
    assert_eq!(
        path.url(&path.bucket_path()),
        "https://store.example.com/logs"
    );

    let virtual_host = endpoint(false);
    assert_eq!(virtual_host.host(), "logs.store.example.com");
    assert_eq!(virtual_host.bucket_path(), "/");
    assert_eq!(virtual_host.object_path(key), format!("/{encoded}"));
    assert_eq!(
        virtual_host.url(&virtual_host.object_path(key)),
        format!("https://logs.store.example.com/{encoded}")
    );

    let local = Endpoint::parse("http://127.0.0.1:9000/", "us-east-1", "logs", true).unwrap();
    assert_eq!(local.url("/logs/k"), "http://127.0.0.1:9000/logs/k");
    assert_eq!(
        (local.region(), local.bucket(), local.path_style()),
        ("us-east-1", "logs", true)
    );

    // The base URL names the store alone, in either style, and reads back to the endpoint.
    assert_eq!(local.base_url(), "http://127.0.0.1:9000");
    assert_eq!(path.base_url(), "https://store.example.com");
    assert_eq!(virtual_host.base_url(), "https://store.example.com");
    for given in [&local, &path, &virtual_host] {
        let again = Endpoint::parse(
            &given.base_url(),
            given.region(),
            given.bucket(),
            given.path_style(),
        );
        assert_eq!(again.as_ref(), Ok(given));
    }

    for path_style in [true, false] {
        let rig = Rig::with_style(path_style);
        let store = rig.admin();
        assert_eq!(store.endpoint().path_style(), path_style);
        store
            .put(key, b"bytes".to_vec(), &Default::default())
            .unwrap();
        assert_eq!(rig.fake.objects().keys().collect::<Vec<_>>(), [key]);
        let object = store.get_range(key, 0, 5).unwrap().unwrap();
        assert_eq!(object.bytes, b"bytes");
        let page = store.list("js/", None, 10).unwrap();
        assert_eq!(page.entries[0].key, key);
    }
}

/// Asserts that an endpoint URL with a user, a query, a fragment, a path, another scheme, no
/// host, an odd host character, too many bytes, or plain http to a host that is not a loopback
/// literal is refused with its rule, and so are a bad region and a bad bucket; and that
/// `https`, and `http` to a loopback literal with or without a port, are accepted.
#[test]
fn an_endpoint_is_refused_by_the_rule_it_breaks() {
    let refused = [
        ("https://user@store.example.com", EndpointError::Userinfo),
        ("https://store.example.com/?x=1", EndpointError::Query),
        ("https://store.example.com#top", EndpointError::Fragment),
        ("https://store.example.com/bucket", EndpointError::Path),
        ("ftp://store.example.com", EndpointError::Scheme),
        ("store.example.com", EndpointError::Scheme),
        ("https://", EndpointError::Host),
        ("https:///", EndpointError::Host),
        ("https://store example.com", EndpointError::Host),
        ("https://store\\example.com", EndpointError::Host),
        ("https://store\texample.com", EndpointError::Host),
        (
            "http://store.example.com",
            EndpointError::PlainHttpNotLoopback,
        ),
        ("http://10.0.0.1:9000", EndpointError::PlainHttpNotLoopback),
        ("http://localhost:9000", EndpointError::PlainHttpNotLoopback),
    ];
    for (url, error) in refused {
        assert_eq!(
            Endpoint::parse(url, "us-east-1", "logs", true),
            Err(error),
            "{url}"
        );
    }
    let long = format!("https://{}", "a".repeat(505));
    assert_eq!(long.len(), 513);
    assert_eq!(
        Endpoint::parse(&long, "us-east-1", "logs", true),
        Err(EndpointError::TooLong)
    );
    let at_limit = format!("https://{}", "a".repeat(504));
    assert!(Endpoint::parse(&at_limit, "us-east-1", "logs", true).is_ok());

    let long_region = "r".repeat(65);
    for region in ["", "US-EAST-1", "us_east_1", long_region.as_str()] {
        assert_eq!(
            Endpoint::parse("https://s.example.com", region, "logs", true),
            Err(EndpointError::Region),
            "{region}"
        );
    }
    let long_bucket = "b".repeat(64);
    for bucket in ["ab", "Logs", "logs_1", long_bucket.as_str()] {
        assert_eq!(
            Endpoint::parse("https://s.example.com", "us-east-1", bucket, true),
            Err(EndpointError::Bucket),
            "{bucket}"
        );
    }
    for url in [
        "https://store.example.com",
        "https://store.example.com:8443/",
        "http://127.0.0.1",
        "http://127.0.0.1:9000",
        "http://[::1]:9000",
    ] {
        assert!(
            Endpoint::parse(url, "us-east-1", "my.logs-1", false).is_ok(),
            "{url}"
        );
    }
}

/// Asserts the text of every endpoint error: the rule, never the URL.
#[test]
fn endpoint_errors_name_the_rule() {
    let texts = [
        (
            EndpointError::TooLong,
            "the endpoint URL is longer than 512 bytes",
        ),
        (
            EndpointError::Scheme,
            "the endpoint URL must begin with https:// or http://",
        ),
        (
            EndpointError::Userinfo,
            "the endpoint URL must not name a user",
        ),
        (
            EndpointError::Query,
            "the endpoint URL must not have a query",
        ),
        (
            EndpointError::Fragment,
            "the endpoint URL must not have a fragment",
        ),
        (EndpointError::Path, "the endpoint URL must not have a path"),
        (
            EndpointError::Host,
            "the endpoint URL's host is empty or holds a character a host cannot",
        ),
        (
            EndpointError::PlainHttpNotLoopback,
            "plain http is accepted for a loopback address only",
        ),
        (
            EndpointError::Region,
            "the region must be 1 to 64 bytes of [a-z0-9-]",
        ),
        (
            EndpointError::Bucket,
            "the bucket must be 3 to 63 bytes of [a-z0-9.-]",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
}

/// Asserts that a credential is valid up to the instant before its expiry and expired from it.
#[test]
fn a_credential_expires_at_its_instant() {
    let credential = credential("AKIDTEST", None);
    let expiry = credential.expires_at;
    assert!(!credential.is_expired(expiry - Duration::from_millis(1)));
    assert!(credential.is_expired(expiry));
    assert!(credential.is_expired(expiry + Duration::from_secs(1)));
}

/// Asserts that a manual clock reads what it was set to and moves only when advanced, also
/// through an `Arc`, and that the system clock reads a time after the start of the tests' fixed
/// clock's epoch.
#[test]
fn clocks_read_what_they_should() {
    let clock = Arc::new(ManualClock::new(at(100)));
    assert_eq!(clock.now(), at(100));
    clock.advance(Duration::from_secs(5));
    assert_eq!(Clock::now(&clock), at(105));
    assert!(SystemClock.now() > at(0));
}
