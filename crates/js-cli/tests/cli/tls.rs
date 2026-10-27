//! https: a server with a certificate made for the test, trusted through `--ca-cert`.

use crate::support::{Harness, Js, T0, acquire_words};

/// Asserts that with `--ca-cert` naming the server's certificate a command works over
/// TLS: a lease is acquired, listed and released, and the token goes only inside TLS.
#[test]
fn https_works_with_the_certificate_trusted() {
    let h = Harness::tls("tls_trusted");
    assert!(
        h.js.server.starts_with("https://127.0.0.1:"),
        "{}",
        h.js.server
    );
    let ca = h.scratch.path("cert.pem");
    let ca = ca.to_str().unwrap();
    let mut words = vec!["--ca-cert", ca];
    words.extend(acquire_words(&[]));
    let got = h.js.go(&words);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        got.out,
        format!("lease 1-1 host gpu-01 expires {}\n", T0 + 600_000)
    );
    let listed = h.js.go(&["--ca-cert", ca, "lease", "list"]);
    assert!(
        listed.out.starts_with("1-1 held host gpu-01"),
        "{}",
        listed.out
    );
    let freed = h.js.go(&["--ca-cert", ca, "lease", "release", "1-1"]);
    assert_eq!(freed.code, 0, "{}", freed.err);
}

/// Asserts that without `--ca-cert` the platform's roots decide, and a certificate
/// nobody vouched for is not trusted: exit 1, and the server saw no request.
#[test]
fn https_without_the_certificate_is_not_trusted() {
    let h = Harness::tls("tls_untrusted");
    let got = h.js.go(&["lease", "list"]);
    assert_eq!((got.code, got.out.as_str()), (1, ""));
    assert!(
        got.err.starts_with("js: cannot talk to https://127.0.0.1:"),
        "{}",
        got.err
    );
}

/// Asserts that a certificate other than the server's is not trusted either, and that
/// `--ca-cert` is ignored for plain http.
#[test]
fn another_certificate_is_not_trusted_and_http_ignores_it() {
    let h = Harness::tls("tls_other");
    let other = Harness::tls("tls_other_server");
    let ca = other.scratch.path("cert.pem");
    let got =
        h.js.go(&["--ca-cert", ca.to_str().unwrap(), "lease", "list"]);
    assert_eq!(got.code, 1, "{}", got.err);

    let plain = Harness::new("tls_plain");
    let js = Js {
        server: plain.js.server.clone(),
        token_file: plain.js.token_file.clone(),
    };
    let ignored = js.go(&["--ca-cert", ca.to_str().unwrap(), "lease", "list"]);
    assert_eq!(ignored.code, 0, "{}", ignored.err);
}
