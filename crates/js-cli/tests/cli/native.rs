//! `lease native`: queues a native job on a held lease. The tests run the CLI against a
//! server that records each request it reads (the request line and the JSON body) and
//! answers with a canned reply, so they check the bytes the command sends and how it
//! tells each answer.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::support::{Scratch, against, http};

const BUNDLE: &str =
    "toolchain:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// What the recording server read: the request line and the parsed body of each request.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A server that answers every request with `reply` and records what it read.
fn recording(reply: String) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let (line, body) = read(&mut stream);
            log.lock().unwrap().push((line, body));
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

/// Reads one request: its first line and its body as JSON.
fn read(stream: &mut impl Read) -> (String, Value) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .find_map(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    let _ = stream.read_exact(&mut body);
    let line = head.lines().next().unwrap().to_owned();
    (line, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

const QUEUED: &str = r#"{"run_id":"run-1-1","state":"queued"}"#;

/// The words of `lease native` for lease 1-1 and the usual limits, plus `extra`.
fn words<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec![
        "lease",
        "native",
        "1-1",
        "--cpu-millis",
        "1000",
        "--memory-bytes",
        "1048576",
    ];
    words.extend(extra);
    words
}

/// Asserts the request of a native job without a bundle: a POST to the lease's runs route
/// whose body is the native object (program, arguments in order, network), the limits,
/// no `image`, no `bundle` and no wall limit; and that the run is printed as text. It
/// catches a body of another shape and a key that is sent when it was not asked for.
#[test]
fn a_native_job_is_queued_with_the_right_body() {
    let scratch = Scratch::new("native_body");
    let (server, seen) = recording(http("202 Accepted", QUEUED));
    let js = against(&server, &scratch);
    let args = [
        "--program",
        "/usr/bin/true",
        "--arg",
        "a",
        "--arg",
        "b c",
        "--network",
        "loopback",
    ];
    let got = js.go(&words(&args));
    assert_eq!(
        (got.code, got.out.as_str()),
        (0, "run run-1-1 queued\n"),
        "{}",
        got.err
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "POST /v1/leases/1-1/runs HTTP/1.1");
    assert_eq!(
        seen[0].1,
        json!({
            "native": { "program": "/usr/bin/true", "args": ["a", "b c"], "network": "loopback" },
            "limits": { "cpu_millis": 1000, "memory_bytes": 1048576 },
        })
    );
}

/// Asserts that the network defaults to off, that `--bundle` and `--wall` are sent when
/// given, and that `--json` prints the server's run as one object.
#[test]
fn the_bundle_and_the_wall_time_are_sent_when_given() {
    let scratch = Scratch::new("native_bundle");
    let (server, seen) = recording(http("202 Accepted", QUEUED));
    let js = against(&server, &scratch);
    let args = [
        "--program",
        "@bundle/clang",
        "--bundle",
        BUNDLE,
        "--wall",
        "90s",
    ];
    let mut all = vec!["--json"];
    all.extend(words(&args));
    let got = js.go(&all);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        got.json(),
        json!({ "run_id": "run-1-1", "state": "queued" })
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].1,
        json!({
            "native": { "program": "@bundle/clang", "args": [], "network": "off" },
            "bundle": BUNDLE,
            "limits": { "cpu_millis": 1000, "memory_bytes": 1048576, "wall_ms": 90000 },
        })
    );
}

/// Asserts that each network level of `--network` is sent under its own name.
#[test]
fn each_network_level_is_sent_by_name() {
    for level in ["off", "loopback", "on"] {
        let scratch = Scratch::new(&format!("native_network_{level}"));
        let (server, seen) = recording(http("202 Accepted", QUEUED));
        let js = against(&server, &scratch);
        let got = js.go(&words(&["--program", "/usr/bin/true", "--network", level]));
        assert_eq!(got.code, 0, "{level}: {}", got.err);
        assert_eq!(seen.lock().unwrap()[0].1["native"]["network"], level);
    }
}

/// Asserts the exit status of each answer: 409 is a refusal (3) with the code, 422 and
/// 400 reject the request (4), 404 is a refusal, and anything else is a server failure
/// (1); and that a 202 whose body has no run id is a server failure too.
#[test]
fn each_answer_has_its_status() {
    let cases = [
        (
            "409 Conflict",
            r#"{"error":"native-unsupported"}"#,
            3,
            "js: refused: native-unsupported\n",
        ),
        (
            "404 Not Found",
            r#"{"error":"unknown-lease"}"#,
            3,
            "js: refused: unknown-lease\n",
        ),
        (
            "422 Unprocessable Entity",
            r#"{"error":"invalid-bundle"}"#,
            4,
            "js: rejected: invalid-bundle\n",
        ),
        (
            "400 Bad Request",
            r#"{"error":"bad-request","reason":"no"}"#,
            4,
            "js: rejected: bad-request: no\n",
        ),
        (
            "500 Internal Server Error",
            r#"{"error":"boom"}"#,
            1,
            "js: unexpected answer 500 (boom)\n",
        ),
        (
            "202 Accepted",
            r#"{"state":"queued"}"#,
            1,
            "js: the server's answer has no usable \"run_id\"\n",
        ),
    ];
    for (n, (status, body, code, message)) in cases.into_iter().enumerate() {
        let scratch = Scratch::new(&format!("native_status_{n}"));
        let (server, _) = recording(http(status, body));
        let got = against(&server, &scratch).go(&words(&["--program", "/usr/bin/true"]));
        assert_eq!((got.code, got.err.as_str()), (code, message), "{status}");
        assert_eq!(got.out, "", "{status}");
    }
}

/// Asserts that a command line the CLI refuses before any request is exit 2 and sends
/// nothing: no program, an unknown network level, a lease id that is not one, and a
/// zero wall time.
#[test]
fn a_bad_command_line_sends_nothing() {
    let scratch = Scratch::new("native_usage");
    let (server, seen) = recording(http("202 Accepted", QUEUED));
    let js = against(&server, &scratch);
    let refused: [Vec<&str>; 4] = [
        words(&[]),
        words(&["--program", "/usr/bin/true", "--network", "everywhere"]),
        vec![
            "lease",
            "native",
            "x",
            "--program",
            "/usr/bin/true",
            "--cpu-millis",
            "1",
            "--memory-bytes",
            "1",
        ],
        words(&["--program", "/usr/bin/true", "--wall", "0"]),
    ];
    for command in refused {
        assert_eq!(js.go(&command).code, 2, "{command:?}");
    }
    assert!(seen.lock().unwrap().is_empty());
}
