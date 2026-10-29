//! Building the client and the mode: the base URL, the tailnet, the tag.

use std::time::Duration;

use js_elastic::Secret;
use js_lease_server::network::{
    API_BASE, NetworkError, OAuthClient, TailscaleApi, TailscaleMode, TailscaleSettings,
};

use crate::support::{binary, clock, unused_api};

fn api(base: &str, tailnet: &str) -> Result<TailscaleApi, NetworkError> {
    let (_, shared) = clock();
    let client = OAuthClient::new("id", Secret::new("secret".to_owned()));
    TailscaleApi::new(base, tailnet, client, shared, Duration::from_secs(1))
}

/// Asserts which base URLs are accepted: https to any host with no path, http to a
/// loopback host only (an address, a bracketed IPv6 address or `localhost`), and the
/// real API's own address; and that every other form is refused as misconfigured:
/// another scheme, credentials in the URL, a path, a trailing slash, a query, a non-loopback http host, and text
/// that is no URL.
#[test]
fn a_base_url_is_https_or_loopback_http_with_no_path() {
    for ok in [
        API_BASE,
        "https://tailscale.example.org:8443",
        "http://127.0.0.1:9",
        "http://[::1]:9",
        "http://localhost:9",
    ] {
        assert!(api(ok, "-").is_ok(), "{ok}");
    }
    for bad in [
        "ftp://example.org",
        "https://example.org/api",
        "https://example.org/",
        "https://example.org?x=1",
        "https://user:pw@example.org",
        "http://example.org",
        "http://10.0.0.1:9",
        "http://127.0.0.1:9/x",
        "example.org",
        "not a url",
        "",
    ] {
        assert!(
            matches!(api(bad, "-"), Err(NetworkError::Misconfigured(_))),
            "{bad:?}"
        );
    }
}

/// Asserts that the refusal of a base URL does not repeat the URL, which may carry a
/// password (`https://user:secret@host`) the refusal would write to the log and the
/// status page. It catches a message that echoes what the operator wrote.
#[test]
fn a_refused_base_url_is_not_echoed() {
    let Err(NetworkError::Misconfigured(said)) = api("https://user:hunter2@example.org/x", "-")
    else {
        panic!("a URL with a path is refused");
    };
    assert!(!said.contains("hunter2"), "{said}");
    assert!(said.contains("must be https://host"), "{said}");
    let Err(NetworkError::Misconfigured(said)) = api("https://user:hunter2@example.org", "-")
    else {
        panic!("a URL with credentials is refused");
    };
    assert!(!said.contains("hunter2"), "{said}");
}

/// Asserts that a tailnet is `-` or a name of letters, digits, `.`, `_` and `-`, and that
/// one that could change the request path (a slash, a space, empty) is refused.
#[test]
fn a_tailnet_must_stand_in_a_path() {
    for ok in ["-", "example.com", "corp_net-1"] {
        assert!(api("https://example.org", ok).is_ok(), "{ok}");
    }
    for bad in ["", "a/b", "a b", "../x", "a?b"] {
        assert!(
            matches!(
                api("https://example.org", bad),
                Err(NetworkError::Misconfigured(_))
            ),
            "{bad:?}"
        );
    }
}

/// Asserts that a tag is `tag:` and a name, and that anything else (no prefix, empty
/// name, a space or a colon in it) is refused with the tag in the message.
#[test]
fn a_tag_is_tag_colon_and_a_name() {
    for ok in ["tag:js-agent", "tag:a1"] {
        let settings = TailscaleSettings {
            tag: ok.to_owned(),
            binary: binary(),
        };
        assert!(TailscaleMode::new(unused_api(), settings).is_ok(), "{ok}");
    }
    for bad in ["js-agent", "tag:", "tag:js agent", "tag:a:b", "TAG:x y", ""] {
        let settings = TailscaleSettings {
            tag: bad.to_owned(),
            binary: binary(),
        };
        let err = TailscaleMode::new(unused_api(), settings).unwrap_err();
        assert!(
            matches!(&err, NetworkError::Misconfigured(m) if m.contains("tag")),
            "{bad:?}"
        );
    }
}
