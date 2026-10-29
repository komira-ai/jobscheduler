//! The default mode adds nothing to a launch and checks that the advertised URL is one a
//! host can dial.

use std::path::PathBuf;

use js_lease_server::network::{DefaultMode, NetworkError, NetworkMode};

use crate::support::{Scratch, TEN_MINUTES, ctx, host};

const URL: &str = "https://sched.example.org:7301";

/// Asserts that the default mode contributes no step and no secret, describes no step,
/// and forgets a host without a call.
#[test]
fn the_default_mode_contributes_nothing() {
    let mode = DefaultMode::new(URL, Vec::new());
    let host = host();
    let launch = ctx(&host, TEN_MINUTES);
    let contribution = mode.prepare(&launch).unwrap();
    assert!(contribution.steps.is_empty());
    assert!(contribution.secrets.is_empty());
    assert!(mode.describe(&launch).is_empty());
    mode.forget(&host).unwrap();
}

/// Asserts that an `http` or `https` URL with a host passes, and that anything else (no
/// scheme, another scheme, no host, text that is no URL) is refused naming the URL.
#[test]
fn the_advertised_url_must_be_http_or_https_with_a_host() {
    for ok in [URL, "http://127.0.0.1:7301", "https://sched.example.org"] {
        DefaultMode::new(ok, Vec::new()).check().unwrap();
    }
    for bad in [
        "sched.example.org:7301",
        "ftp://sched.example.org",
        "https://",
        "not a url",
        "",
    ] {
        let err = DefaultMode::new(bad, Vec::new()).check().unwrap_err();
        assert!(
            matches!(&err, NetworkError::Misconfigured(m) if m.contains(&format!("{bad:?}"))),
            "{bad:?}: {err}"
        );
    }
}

/// Asserts that for `https` the certificate files must open, and the first that does not
/// is named; and that for `http` they are not looked at.
#[test]
fn an_https_service_needs_its_certificate_files() {
    let scratch = Scratch::new("default-mode");
    let cert = scratch.write("cert.pem", b"pem", 0o600);
    let key = scratch.write("key.pem", b"pem", 0o600);
    DefaultMode::new(URL, vec![cert.clone(), key])
        .check()
        .unwrap();

    let absent: PathBuf = cert.with_file_name("absent.pem");
    let err = DefaultMode::new(URL, vec![cert, absent.clone()])
        .check()
        .unwrap_err();
    assert!(
        matches!(&err, NetworkError::Misconfigured(m) if m.contains("absent.pem")),
        "{err}"
    );

    DefaultMode::new("http://127.0.0.1:7301", vec![absent])
        .check()
        .unwrap();
}
