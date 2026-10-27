//! The server address: https anywhere, http only to the machine itself.

use js_client::ServerUrl;

/// Asserts the addresses that are accepted, with the base each gives (no trailing slash).
#[test]
fn https_anywhere_and_http_to_loopback_are_accepted() {
    for (text, base, https) in [
        ("https://example.com", "https://example.com", true),
        (
            "https://example.com:8443/",
            "https://example.com:8443",
            true,
        ),
        ("http://localhost:8080", "http://localhost:8080", false),
        ("http://LocalHost", "http://LocalHost", false),
        ("http://127.0.0.1:9", "http://127.0.0.1:9", false),
        ("http://127.1.2.3", "http://127.1.2.3", false),
        ("http://[::1]:9", "http://[::1]:9", false),
    ] {
        let got = ServerUrl::parse(text).unwrap();
        assert_eq!((got.base.as_str(), got.https), (base, https), "{text}");
    }
}

/// Asserts each rule's refusal and message.
#[test]
fn every_other_address_is_refused_with_its_reason() {
    for (text, why) in [
        (
            "http://example.com",
            "is plain http to a host off the loopback: a bearer token never crosses a network in clear, use https://",
        ),
        (
            "http://10.0.0.1:80",
            "is plain http to a host off the loopback: a bearer token never crosses a network in clear, use https://",
        ),
        ("https://user@example.com", "must not carry a user"),
        ("http://user:pw@localhost", "must not carry a user"),
        (
            "https://example.com/path",
            "must be scheme://host:port with no path or query",
        ),
        (
            "https://example.com?x=1",
            "must be scheme://host:port with no path or query",
        ),
        (
            "ftp://example.com",
            "must start with https:// (or http:// for a loopback host)",
        ),
        (
            "example.com:80",
            "must start with https:// (or http:// for a loopback host)",
        ),
    ] {
        assert_eq!(
            ServerUrl::parse(text).unwrap_err().to_string(),
            format!("--server: {text:?} {why}")
        );
    }
    for text in ["", "not a url"] {
        let said = ServerUrl::parse(text).unwrap_err().to_string();
        assert!(
            said.starts_with(&format!("--server: {text:?} is not a URL: ")),
            "{said}"
        );
    }
}
