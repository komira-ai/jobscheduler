//! The bearer token in front of every route but health.

use crate::support::{Harness, TOKEN, exchange_authorized, request};

/// Asserts that a request with no token, a wrong token, a token of another length, a
/// token under another scheme or a token in another header is a 401 with an empty body,
/// on every kind of route including one that does not exist, and that the right token
/// passes.
#[tokio::test]
async fn token_is_required() {
    let h = Harness::new("auth").await;
    let addr = h.addr();
    let wrong_same_length = TOKEN.replace('t', "T");
    let routes = [
        ("GET", "/v1/leases"),
        ("POST", "/v1/leases"),
        ("POST", "/v1/leases/1-1/renew"),
        ("DELETE", "/v1/leases/1-1"),
        ("GET", "/v1/unknown"),
    ];
    for (method, path) in routes {
        for token in [
            None,
            Some("nope"),
            Some(wrong_same_length.as_str()),
            Some(""),
        ] {
            let reply = request(addr, method, path, token, None).await;
            assert_eq!(reply.status, 401, "{method} {path} {token:?}");
            assert_eq!(reply.body, "");
        }
    }
    let basic = raw_header(addr, "Authorization: Basic dGVzdDp0ZXN0").await;
    assert_eq!(basic, 401);
    let other = raw_header(addr, &format!("X-Token: {TOKEN}")).await;
    assert_eq!(other, 401);
    let right = request(addr, "GET", "/v1/leases", Some(TOKEN), None).await;
    assert_eq!(right.status, 200);
}

/// Sends a GET of the list with one header line of the caller's choosing.
async fn raw_header(addr: std::net::SocketAddr, header: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let text =
        format!("GET /v1/leases HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{header}\r\n\r\n");
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    crate::support::parse_reply(&raw).status
}

/// Asserts that the Bearer scheme is accepted in any letter case with the right token
/// and still refused with a wrong one (RFC 9110: an authentication scheme is not case
/// sensitive).
#[tokio::test]
async fn the_scheme_is_case_insensitive() {
    let h = Harness::new("auth_scheme_case").await;
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let stream = tokio::net::TcpStream::connect(h.addr()).await.unwrap();
        let value = format!("{scheme} {TOKEN}");
        let reply = exchange_authorized(stream, "GET", "/v1/leases", Some(&value), None).await;
        assert_eq!(reply.status, 200, "{scheme}");
        let stream = tokio::net::TcpStream::connect(h.addr()).await.unwrap();
        let wrong = format!("{scheme} nope");
        let reply = exchange_authorized(stream, "GET", "/v1/leases", Some(&wrong), None).await;
        assert_eq!(reply.status, 401, "{scheme}");
    }
}
