//! The loopback API client: the call printer, the answer reader, and the real client against a
//! listener thread on `127.0.0.1`.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread::JoinHandle;

use common::{FakeApi, FakeNet, answer, ok, with_api};
use js_host_step::Mode;
use js_host_step::fake::FakeHost;
use js_mdm::api::{self, Call, Method, RealApi, Reply, Service, parse_reply};
use js_mdm::secret;
use js_mdm::{Env, MdmError};

const KEY: &str = "k3y-for-tests";
/// base64 of `depserver:k3y-for-tests`.
const AUTH: &str = "Basic ZGVwc2VydmVyOmszeS1mb3ItdGVzdHM=";

fn key(host: &FakeHost) -> secret::Secret {
    host.put_file("/k.env", 0o600, (0, 0), format!("K={KEY}\n").as_bytes());
    secret::read(host, "/k.env", "K").unwrap()
}

fn service(host: &FakeHost, addr: &str) -> Result<Service, MdmError> {
    let addr: SocketAddr = addr.parse().unwrap();
    Service::new("NanoDEP", "depserver", addr, key(host))
}

/// Catches an API key that could be sent off the host: a service address that is not
/// loopback is refused (exit 2) and the message names it; `127.0.0.1`, another address of
/// `127/8` and `::1` are accepted; the service's `Debug` shows its name and address and never
/// the key.
#[test]
fn a_service_is_loopback_only() {
    let host = FakeHost::new(0);
    for bad in [
        "10.0.0.5:9001",
        "192.168.1.250:9001",
        "0.0.0.0:9001",
        "[2001:db8::1]:9001",
    ] {
        let err = service(&host, bad).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{bad}");
        assert_eq!(
            err.to_string(),
            format!("{bad} is not a loopback address: the API keys never leave the host")
        );
    }
    for good in ["127.0.0.1:9001", "127.0.0.2:80", "[::1]:9001"] {
        let s = service(&host, good).unwrap();
        assert_eq!(s.addr().to_string(), good);
        assert_eq!(s.name(), "NanoDEP");
        let shown = format!("{s:?}");
        assert!(shown.contains(good) && !shown.contains(KEY), "{shown}");
        assert_eq!(s.authorization(), AUTH);
    }
}

/// Catches a `Debug` of a call that shows its body (a private key or token travels in
/// some): it prints the method, the path and the size.
#[test]
fn a_calls_debug_shows_method_path_and_size_only() {
    let post = Call::post_json("/proxy/x/profile", b"SECRET BODY".to_vec());
    assert_eq!(
        format!("{post:?}"),
        "Call { POST \"/proxy/x/profile\", body: 11 bytes }"
    );
    let put = Call::put("/v1/assigner/x?profile_uuid=U", Vec::new());
    assert_eq!(
        format!("{put:?}"),
        "Call { PUT \"/v1/assigner/x?profile_uuid=U\", body: 0 bytes }"
    );
    assert_eq!((put.method, post.method), (Method::Put, Method::Post));
}

/// Catches an answer read wrongly: a body by `Content-Length` (bytes after it ignored), a
/// chunked body (with an extension and several chunks), a body that runs to the end, a header
/// name in any case, and a `Transfer-Encoding` other than chunked falling back to the length.
#[test]
fn answers_are_read_by_length_by_chunks_or_to_the_end() {
    let reply = |raw: &[u8]| parse_reply(raw).unwrap();
    let said = |status, body: &str| Reply {
        status,
        body: body.as_bytes().to_vec(),
    };
    assert_eq!(
        reply(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA"),
        said(200, "hello")
    );
    assert_eq!(
        reply(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n"),
        said(403, "")
    );
    assert_eq!(
        reply(b"HTTP/1.1 200 OK\r\nTRANSFER-ENCODING: Chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
        said(200, "hello world")
    );
    assert_eq!(
        reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: identity\r\nContent-Length: 2\r\n\r\nok"),
        said(200, "ok")
    );
    assert_eq!(
        reply(b"HTTP/1.1 500 Oops\r\nServer: x\r\n\r\nto the end"),
        said(500, "to the end")
    );
}

/// Catches an answer taken on trust when it is cut or wrong: no end of headers, no status, a
/// length that is not a number or longer than the body, and a chunked body with a bad size, a
/// size line that never ends, a chunk cut short or one that does not end in a line break are
/// each an error that says which.
#[test]
fn damaged_answers_are_errors() {
    for (raw, why) in [
        (
            &b"HTTP/1.1 200 OK\r\nContent-Length: 2"[..],
            "no end of headers",
        ),
        (b"garbage\r\n\r\n", "no status"),
        (b"HTTP/1.1 abc\r\n\r\n", "no status"),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\nhi",
            "length is not a number",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhi",
            "shorter than it says",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhi\r\n0\r\n\r\n",
            "chunk size is not a number",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2",
            "chunk size has no end",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
            "chunk is cut short",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab",
            "does not end where it says",
        ),
    ] {
        let err = parse_reply(raw).unwrap_err();
        assert!(err.contains(why), "{}: {err}", String::from_utf8_lossy(raw));
    }
}

/// Catches an error that quotes too much or too little of an answer: 300 characters at most,
/// trimmed, whole characters (not bytes).
#[test]
fn quoted_answers_are_trimmed_and_capped() {
    assert_eq!(api::quoted(b"  no such name \n"), "no such name");
    assert_eq!(api::quoted("é".repeat(400).as_bytes()), "é".repeat(300));
}

fn run_call(
    host: &FakeHost,
    api: &dyn api::Api,
    service: &Service,
    call: &Call,
    mode: Mode,
) -> (Result<Option<Vec<u8>>, MdmError>, String) {
    let net = FakeNet::new(Default::default());
    let mut out = Vec::new();
    let result = with_api(host, &net, api, |env: &Env<'_>| {
        api::call(env, service, call, mode, &mut out)
    });
    (result, String::from_utf8(out).unwrap())
}

/// Catches a call printed with its body or in the wrong form, a dry run that makes the call, a
/// 2xx answer whose body is lost, and a refusal taken for success: the printed lines are
/// `DRY-RUN: <call>` / `+ <call>` with the size and never the body; 2xx returns the body; 404
/// and 300 are errors quoting the answer (exit 1).
#[test]
fn calls_are_printed_without_bodies_and_checked() {
    let host = FakeHost::new(0);
    let svc = service(&host, "127.0.0.1:9001").unwrap();
    let api = FakeApi::new(|call| match call.path.as_str() {
        "/missing" => answer(404, "no such name"),
        "/moved" => answer(300, "elsewhere"),
        _ => ok("{\"a\":1}"),
    });
    let put = Call::put("/x", b"PRIVATE KEY".to_vec());
    let (result, out) = run_call(&host, &api, &svc, &put, Mode::DryRun);
    assert_eq!(result.unwrap(), None);
    assert_eq!(
        out,
        "DRY-RUN: PUT http://127.0.0.1:9001/x (NanoDEP API, 11 bytes)\n"
    );
    assert!(api.sent.borrow().is_empty());

    let (result, out) = run_call(&host, &api, &svc, &put, Mode::Apply);
    assert_eq!(result.unwrap(), Some(b"{\"a\":1}".to_vec()));
    assert_eq!(
        out,
        "+ PUT http://127.0.0.1:9001/x (NanoDEP API, 11 bytes)\n"
    );
    assert_eq!(*api.authorizations.borrow(), [AUTH]);

    let empty = Call::put("/missing", Vec::new());
    let (result, out) = run_call(&host, &api, &svc, &empty, Mode::Apply);
    assert_eq!(out, "+ PUT http://127.0.0.1:9001/missing (NanoDEP API)\n");
    let err = result.unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert_eq!(
        err.to_string(),
        "PUT http://127.0.0.1:9001/missing (NanoDEP API): answered 404: no such name"
    );
    let (result, _) = run_call(&host, &api, &svc, &Call::put("/moved", vec![]), Mode::Apply);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .ends_with("answered 300: elsewhere")
    );
}

/// What a listener thread saw of one request.
struct Seen {
    head: Vec<String>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.iter().skip(1).find_map(|l| {
            let (n, v) = l.split_once(": ")?;
            n.eq_ignore_ascii_case(name).then_some(v)
        })
    }
}

/// Serves `answers` to as many connections, one each, and returns what it saw.
fn serve(answers: Vec<Vec<u8>>) -> (SocketAddr, JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for answer in answers {
            let (mut tcp, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(tcp.try_clone().unwrap());
            let mut head = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_owned();
                if line.is_empty() {
                    break;
                }
                head.push(line);
            }
            let mut got = Seen {
                head,
                body: Vec::new(),
            };
            let length: usize = got.header("content-length").unwrap().parse().unwrap();
            got.body = vec![0; length];
            reader.read_exact(&mut got.body).unwrap();
            tcp.write_all(&answer).unwrap();
            seen.push(got);
        }
        seen
    });
    (addr, thread)
}

/// Catches a client that gets the wire wrong: over a real socket it sends the request line, a
/// `Host`, `Authorization: Basic base64(user:key)` and the exact `Content-Length` and body,
/// `Content-Type: application/json;charset=UTF8` only for a JSON body, and the key nowhere
/// else; it reads an answer by length, chunked, or to the close; and a non-2xx answer is an
/// error (exit 1) that quotes it.
#[test]
fn the_real_client_speaks_http_to_a_loopback_listener() {
    let (addr, thread) = serve(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"b\r\n3\r\n\":2\r\n1\r\n}\r\n0\r\n\r\n".to_vec(),
        b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\nnot yours".to_vec(),
    ]);
    let host = FakeHost::new(0);
    let svc = service(&host, &addr.to_string()).unwrap();
    let net = FakeNet::new(Default::default());
    let env = Env {
        host: &host,
        net: &net,
        api: &RealApi,
        random: &common::fill,
        now: 0,
    };
    let mut out = Vec::new();
    let put = Call::put("/v1/assigner/x?profile_uuid=U", Vec::new());
    let first = api::call(&env, &svc, &put, Mode::Apply, &mut out).unwrap();
    assert_eq!(first, Some(b"{\"a\":1}".to_vec()));
    let post = Call::post_json("/proxy/x/profile", b"{\"k\":\"v\"}".to_vec());
    let second = api::call(&env, &svc, &post, Mode::Apply, &mut out).unwrap();
    assert_eq!(second, Some(b"{\"b\":2}".to_vec()));
    let err = api::call(&env, &svc, &post, Mode::Apply, &mut out).unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string().ends_with("answered 403: not yours"),
        "{err}"
    );
    let seen = thread.join().unwrap();

    assert_eq!(
        seen[0].head[0],
        "PUT /v1/assigner/x?profile_uuid=U HTTP/1.1"
    );
    assert_eq!(seen[0].header("authorization"), Some(AUTH));
    assert_eq!(seen[0].header("host"), Some(addr.to_string().as_str()));
    assert_eq!(seen[0].header("content-length"), Some("0"));
    assert_eq!(seen[0].header("content-type"), None);
    assert_eq!(seen[0].header("connection"), Some("close"));
    assert!(seen[0].body.is_empty());
    assert_eq!(seen[1].head[0], "POST /proxy/x/profile HTTP/1.1");
    assert_eq!(seen[1].header("authorization"), Some(AUTH));
    assert_eq!(
        seen[1].header("content-type"),
        Some("application/json;charset=UTF8")
    );
    assert_eq!(seen[1].body, b"{\"k\":\"v\"}");
    let key_elsewhere = seen
        .iter()
        .flat_map(|s| s.head.iter().filter(|l| !l.starts_with("Authorization")))
        .any(|l| l.contains(KEY));
    assert!(!key_elsewhere);
    assert!(!String::from_utf8_lossy(&out).contains(KEY));
}

/// Catches a connection failure that is not reported as a failed call: nothing listens on the
/// port, so the call fails (exit 1) naming the call, and an answer that is not HTTP does too.
#[test]
fn the_real_client_reports_a_closed_port_and_a_non_http_answer() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let host = FakeHost::new(0);
    let svc = service(&host, &addr.to_string()).unwrap();
    let put = Call::put("/x", Vec::new());
    let (result, _) = run_call(&host, &RealApi, &svc, &put, Mode::Apply);
    let err = result.unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string()
            .starts_with(&format!("PUT http://{addr}/x (NanoDEP API): ")),
        "{err}"
    );

    let (addr, thread) = serve(vec![b"not http at all".to_vec()]);
    let svc = service(&host, &addr.to_string()).unwrap();
    let (result, _) = run_call(&host, &RealApi, &svc, &put, Mode::Apply);
    thread.join().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .ends_with("the answer has no end of headers")
    );
}
