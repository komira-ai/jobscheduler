//! `UreqTransport` against a server started in the test on 127.0.0.1, port 0, over plain http.

use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use js_logstore::{
    Endpoint, HttpRequest, MAX_RESPONSE_BYTES, ManualClock, Metadata, Method, Store, Transport,
    TransportError, UreqTransport,
};

use crate::server::{Reply, Server};
use crate::support::{REGION, START, at, credential};

/// A limit no answering test meets.
const PATIENT: Duration = Duration::from_secs(30);

/// A request of `method` to `url` with `headers` and `body`.
fn request(method: Method, url: String, headers: &[(&str, &str)], body: &[u8]) -> HttpRequest {
    HttpRequest {
        method,
        url,
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.to_vec(),
    }
}

/// Asserts each documented result of one exchange: a 200 with its body and headers after one
/// request carrying the method, target, headers and body given; a 403 and a 503 returned as
/// answers; a refused connection as `Io(ConnectionRefused)`; a server that accepts and never
/// answers as `Timeout` under the caller's limit of one second (the kind, never how long it
/// took); a redirect returned as it is with its target never asked; a body of exactly the cap
/// read and one byte more `TooLarge`; an answer that is not HTTP `Exchange`; and a header the
/// HTTP library refuses `Request`. Catches a followed redirect, an unbounded read or an
/// unmapped failure.
#[test]
fn the_transport_makes_one_request_and_maps_failures() {
    let transport = UreqTransport::new(PATIENT);

    let ok = Server::start(vec![Reply::http(200, &[("x-amz-meta-a", "1")], b"hello")]);
    let sent = request(
        Method::Put,
        format!("{}/logs/k?x=1", ok.url()),
        &[("host", ok.authority().as_str()), ("x-test", "v")],
        b"abc",
    );
    let answer = transport.send(sent).unwrap();
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, b"hello");
    assert_eq!(answer.header("x-amz-meta-a"), Some("1"));
    let seen = ok.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].line, "PUT /logs/k?x=1 HTTP/1.1");
    assert_eq!(seen[0].header("x-test"), Some("v"));
    assert_eq!(seen[0].header("host"), Some(ok.authority().as_str()));
    assert_eq!(seen[0].body, b"abc");

    for (method, status, verb) in [
        (Method::Get, 403, "GET"),
        (Method::Delete, 503, "DELETE"),
        (Method::Post, 200, "POST"),
    ] {
        let server = Server::start(vec![Reply::http(status, &[], b"<Error/>")]);
        let answer = transport
            .send(request(method, format!("{}/k", server.url()), &[], b""))
            .unwrap();
        assert_eq!(
            (answer.status, answer.body.as_slice()),
            (status, &b"<Error/>"[..])
        );
        assert_eq!(server.seen()[0].line, format!("{verb} /k HTTP/1.1"));
    }

    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_url = format!("http://{}/k", closed.local_addr().unwrap());
    drop(closed);
    assert_eq!(
        transport.send(request(Method::Get, closed_url.clone(), &[], b"")),
        Err(TransportError::Io(io::ErrorKind::ConnectionRefused))
    );

    let silent = Server::start(vec![Reply::Hang]);
    let impatient = UreqTransport::new(Duration::from_secs(1));
    assert_eq!(
        impatient.send(request(
            Method::Get,
            format!("{}/k", silent.url()),
            &[],
            b""
        )),
        Err(TransportError::Timeout)
    );
    assert_eq!(silent.held(), 1);

    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let location = format!("http://{}/elsewhere", target.local_addr().unwrap());
    let redirect = Server::start(vec![Reply::http(
        301,
        &[("location", location.as_str())],
        b"",
    )]);
    let answer = transport
        .send(request(
            Method::Get,
            format!("{}/k", redirect.url()),
            &[],
            b"",
        ))
        .unwrap();
    assert_eq!(answer.status, 301);
    assert_eq!(answer.header("location"), Some(location.as_str()));
    assert_eq!(redirect.seen().len(), 1);
    assert_eq!(
        target.accept().map(|_| ()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    let cap = usize::try_from(MAX_RESPONSE_BYTES).unwrap();
    let at_cap = Server::start(vec![Reply::http(200, &[], &vec![b'a'; cap])]);
    let answer = transport
        .send(request(
            Method::Get,
            format!("{}/k", at_cap.url()),
            &[],
            b"",
        ))
        .unwrap();
    assert_eq!(answer.body.len(), cap);
    let over_cap = Server::start(vec![Reply::http(200, &[], &vec![b'a'; cap + 1])]);
    assert_eq!(
        transport.send(request(
            Method::Get,
            format!("{}/k", over_cap.url()),
            &[],
            b""
        )),
        Err(TransportError::TooLarge)
    );

    let binary = Server::start(vec![Reply::Raw(
        b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\nx-bin: \xff\r\nx-text: t\r\n\r\n".to_vec(),
    )]);
    let answer = transport
        .send(request(
            Method::Get,
            format!("{}/k", binary.url()),
            &[],
            b"",
        ))
        .unwrap();
    assert_eq!(answer.header("x-bin"), None);
    assert_eq!(answer.header("x-text"), Some("t"));

    let garbage = Server::start(vec![Reply::Raw(b"this is not http\r\n\r\n".to_vec())]);
    assert_eq!(
        transport.send(request(
            Method::Get,
            format!("{}/k", garbage.url()),
            &[],
            b""
        )),
        Err(TransportError::Exchange)
    );

    assert_eq!(
        transport.send(request(Method::Get, closed_url, &[("x-bad", "a\nb")], b"")),
        Err(TransportError::Request)
    );
}

/// Asserts a whole call over the real transport: a listing through a path-style `http`
/// endpoint on 127.0.0.1 is one signed GET of the ListObjectsV2 query with the host the URL
/// names, and its answer is read.
#[test]
fn a_store_call_goes_over_the_real_transport() {
    let body = b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>js/a</Key><LastModified>2027-01-15T08:00:00Z</LastModified><Size>3</Size></Contents></ListBucketResult>";
    let server = Server::start(vec![
        Reply::http(200, &[("content-type", "application/xml")], body),
        Reply::http(200, &[], b""),
    ]);
    let endpoint = Endpoint::parse(&server.url(), REGION, "logs", true).unwrap();
    let store = Store::new(
        UreqTransport::new(PATIENT),
        Arc::new(ManualClock::new(at(START))),
        endpoint,
        credential("AKIDTEST", None),
    );
    let page = store.list("js/", None, 10).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        (page.entries[0].key.as_str(), page.entries[0].size),
        ("js/a", 3)
    );
    assert_eq!(page.entries[0].last_modified, at(START));
    store
        .put("js/a", b"abc".to_vec(), &Metadata::new())
        .unwrap();

    let seen = server.seen();
    assert_eq!(
        seen[0].line,
        "GET /logs?list-type=2&max-keys=10&prefix=js%2F HTTP/1.1"
    );
    assert_eq!(seen[0].header("host"), Some(server.authority().as_str()));
    assert!(
        seen[0]
            .header("authorization")
            .unwrap()
            .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date,")
    );
    assert_eq!(seen[1].line, "PUT /logs/js/a HTTP/1.1");
    assert_eq!(seen[1].body, b"abc");
    assert_eq!(
        seen[1].header("x-amz-content-sha256"),
        Some(js_logstore::sigv4::sha256_hex(b"abc").as_str())
    );
}
