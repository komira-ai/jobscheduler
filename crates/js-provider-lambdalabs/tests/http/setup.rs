//! Building a client: the base URL rules, the CA bundle, and what the client prints.

use std::path::Path;

use js_provider_lambdalabs::{ApiKey, HttpApi, SetupError};

use crate::support::{Cert, PATIENT, Scratch};

fn key(scratch: &Scratch) -> ApiKey {
    ApiKey::from_file(&scratch.key_file("setup-key-canary")).unwrap()
}

fn connect(url: &str, bundle: Option<&Path>) -> Result<HttpApi, SetupError> {
    let scratch = Scratch::new("setup-key");
    HttpApi::connect(url, &key(&scratch), bundle, PATIENT)
}

#[test]
fn plain_http_is_accepted_for_loopback_addresses_only() {
    for ok in [
        "http://127.0.0.1:8080",
        "http://127.0.0.1:8080/",
        "http://127.5.5.5:1",
        "http://[::1]:8080",
        "http://127.0.0.1",
    ] {
        assert!(connect(ok, None).is_ok(), "{ok}");
    }
    for refused in [
        "http://192.0.2.10:80",
        "http://203.0.113.5",
        "http://localhost:8080",
        "http://example.test:80",
        "http://0.0.0.0:80",
    ] {
        let err = connect(refused, None).unwrap_err();
        assert!(
            matches!(err, SetupError::PlainHttpNotLoopback(_)),
            "{refused}: {err:?}"
        );
    }
}

#[test]
fn a_base_url_is_scheme_and_host_and_nothing_more() {
    for bad in [
        "",
        "127.0.0.1:8080",
        "ftp://127.0.0.1",
        "http://",
        "https://",
        "http://127.0.0.1:80/api",
        "http://127.0.0.1:80?x=1",
        "http://127.0.0.1:80#frag",
        "http://user@127.0.0.1:80",
        "http://127.0.0.1:1@203.0.113.9",
        "http://127.0.0.1 :80",
        "http://127.0.0.1\n:80",
        "http://127.0.0.1\\@x",
        "https://example.test/api/v1",
    ] {
        assert_eq!(
            connect(bad, None).unwrap_err(),
            SetupError::BaseUrl,
            "{bad:?}"
        );
    }
}

#[test]
fn an_https_base_url_needs_a_ca_bundle_with_a_certificate_in_it() {
    assert_eq!(
        connect("https://example.test", None).unwrap_err(),
        SetupError::CaBundleRequired
    );
    let scratch = Scratch::new("setup-bundles");
    let missing = scratch.path("absent.pem");
    let empty = scratch.write("empty.pem", b"nothing here\n", 0o644);
    let good = scratch.write("good.pem", Cert::new().pem.as_bytes(), 0o644);
    for bundle in [&missing, &empty] {
        let err = connect("https://example.test", Some(bundle)).unwrap_err();
        assert!(
            matches!(&err, SetupError::CaBundle { path, .. } if path == bundle),
            "{err:?}"
        );
    }
    assert!(connect("https://example.test", Some(&good)).is_ok());
}

#[test]
fn a_bundle_is_ignored_for_plain_http() {
    let scratch = Scratch::new("setup-ignored");
    assert!(connect("http://127.0.0.1:1", Some(&scratch.path("absent.pem"))).is_ok());
}

#[test]
fn the_client_prints_its_address_and_never_the_key() {
    let api = connect("http://127.0.0.1:8080/", None).unwrap();
    let shown = format!("{api:?}");
    assert!(shown.contains("http://127.0.0.1:8080"), "{shown}");
    assert!(
        !shown.contains("canary") && !shown.contains("Bearer"),
        "{shown}"
    );
}
