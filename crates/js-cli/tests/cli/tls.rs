//! https: a server with a certificate made for the test, trusted through `--ca-cert`.

use crate::support::{Harness, Js, REVOKE};

/// Asserts that with `--ca-cert` naming the server's certificate a command works over
/// TLS: a revoke is answered and printed, and the token goes only inside TLS.
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
    let got = h.js.go(&[&["--ca-cert", ca], &REVOKE[..]].concat());
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, "join tokens for lambdalabs:i-1 revoked\n");
}

/// Asserts that an `https://` server whose certificate is not from a public authority is
/// refused without `--ca-cert` (exit 1, no output): the bundled public roots do not
/// include the test's certificate.
#[test]
fn https_without_a_ca_cert_trusts_only_public_roots() {
    let h = Harness::tls("tls_no_ca");
    let got = h.js.go(&REVOKE);
    assert_eq!((got.code, got.out.as_str()), (1, ""));
    assert!(got.err.starts_with("js: "), "{}", got.err);
}

/// Asserts that a certificate other than the server's is not trusted either, and that
/// `--ca-cert` is ignored for plain http.
#[test]
fn another_certificate_is_not_trusted_and_http_ignores_it() {
    let h = Harness::tls("tls_other");
    let other = Harness::tls("tls_other_server");
    let ca = other.scratch.path("cert.pem");
    let ca = ca.to_str().unwrap();
    let words = [&["--ca-cert", ca], &REVOKE[..]].concat();
    let got = h.js.go(&words);
    assert_eq!(got.code, 1, "{}", got.err);

    let plain = Harness::new("tls_plain");
    let js = Js {
        server: plain.js.server.clone(),
        token_file: plain.js.token_file.clone(),
    };
    let ignored = js.go(&words);
    assert_eq!(ignored.code, 0, "{}", ignored.err);
}
