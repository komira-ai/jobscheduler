//! The live farm against a server that gives canned answers: the requests it sends, the
//! answers it reads, the refusals and the answers it cannot read.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use js_measure::farm::Standing;
use js_measure::{Acquired, Curve, Farm, FarmError, HostShape, HttpFarm, LeaseAsk, RunAsk};
use js_testkit::dir::TestDir;

use crate::support::job;

/// A canned answer: the request line it is for, the status, the body.
pub type Route = (&'static str, u16, &'static str);

/// A server on the loopback that answers each request line with its canned answer (404
/// for any other) and logs `<request line> <body>`.
pub struct Canned {
    pub url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Canned {
    pub fn serve(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let (log, routes) = (Arc::clone(&log), routes.clone());
                let stream = stream.unwrap();
                thread::spawn(move || answer(stream, &routes, &log));
            }
        });
        Self { url, seen }
    }

    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn answer(mut stream: TcpStream, routes: &[Route], log: &Mutex<Vec<String>>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")?
                .trim()
                .parse()
                .ok()
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    let _ = stream.read_exact(&mut body);
    let line = head
        .lines()
        .next()
        .unwrap()
        .trim_end_matches(" HTTP/1.1")
        .to_owned();
    log.lock()
        .unwrap()
        .push(format!("{line} {}", String::from_utf8_lossy(&body)));
    let (status, text) = routes
        .iter()
        .find(|(route, ..)| *route == line)
        .map_or((404, r#"{"error":"not-found"}"#), |(_, status, text)| {
            (*status, *text)
        });
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(reply.as_bytes());
}

/// The token file `connect` needs, in a directory that lives as long as the guard.
fn token() -> (TestDir, PathBuf) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-measure-http");
    let path = dir.path().join("token");
    std::fs::write(&path, "secret\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (dir, path)
}

fn farm_at(url: &str) -> (TestDir, HttpFarm) {
    let (dir, path) = token();
    (dir, HttpFarm::connect(url, &path, None).unwrap())
}

fn lease_ask() -> LeaseAsk {
    job("a", 1, None).lease
}

fn run_ask() -> RunAsk {
    job("a", 90_000, Some("prof")).run
}

/// Asserts the lease request: what is sent (class, resources, wall time, with the token),
/// 201 read as held with its host and 202 as pending, and a refusal as the server's
/// status and code.
#[test]
fn a_lease_request_and_its_three_answers() {
    let held = Canned::serve(vec![(
        "POST /v1/leases",
        201,
        r#"{"id":"1-1","host":"gpu-01","expires_at_ms":5}"#,
    )]);
    let (_dir, farm) = farm_at(&held.url);
    assert_eq!(
        farm.acquire(&lease_ask()).unwrap(),
        Acquired::Held {
            id: "1-1".to_owned(),
            host: "gpu-01".to_owned()
        }
    );
    let sent = held.seen();
    assert_eq!(sent.len(), 1);
    let body = sent[0].strip_prefix("POST /v1/leases ").unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["class"], "whole-node");
    assert_eq!(body["resources"]["cpu_millis"], 4_000);
    assert_eq!(body["resources"]["memory_bytes"], 8u64 << 30);
    assert_eq!(body["max_wall_seconds"], 3_600);

    let pending = Canned::serve(vec![(
        "POST /v1/leases",
        202,
        r#"{"id":"1-2","state":"pending"}"#,
    )]);
    let (_dir, farm) = farm_at(&pending.url);
    assert_eq!(
        farm.acquire(&lease_ask()).unwrap(),
        Acquired::Pending {
            id: "1-2".to_owned()
        }
    );

    let full = Canned::serve(vec![("POST /v1/leases", 409, r#"{"error":"no-capacity"}"#)]);
    let (_dir, farm) = farm_at(&full.url);
    assert_eq!(
        farm.acquire(&lease_ask()).unwrap_err(),
        FarmError::Refused {
            status: 409,
            code: "no-capacity".to_owned()
        }
    );
}

/// Asserts how the lease list is read: a held lease with its host, a pending one, one in
/// another state (releasing) and one not listed are held, pending, gone and gone.
#[test]
fn a_lease_is_looked_up_in_the_list() {
    let server = Canned::serve(vec![(
        "GET /v1/leases",
        200,
        r#"{"leases":[
          {"id":"1-1","state":"held","host":"h1","acquired_at_ms":1,"expires_at_ms":2,"ends_at_ms":3,"labels":{}},
          {"id":"1-2","state":"pending","acquired_at_ms":1,"expires_at_ms":2,"ends_at_ms":3,"labels":{}},
          {"id":"1-3","state":"releasing","host":"h2","acquired_at_ms":1,"expires_at_ms":2,"ends_at_ms":3,"labels":{}}]}"#,
    )]);
    let (_dir, farm) = farm_at(&server.url);
    assert_eq!(
        farm.lease("1-1").unwrap(),
        Standing::Held {
            host: "h1".to_owned()
        }
    );
    assert_eq!(farm.lease("1-2").unwrap(), Standing::Pending);
    assert_eq!(farm.lease("1-3").unwrap(), Standing::Gone);
    assert_eq!(farm.lease("9-9").unwrap(), Standing::Gone);
}

/// Asserts the run calls: the body that is sent (image, limits, the profile), the run id
/// read from the 202, a run's state and outcome read from its entry, and a release that is
/// accepted (200 or 202) or refused (404).
#[test]
fn runs_are_queued_read_and_released() {
    let server = Canned::serve(vec![
        (
            "POST /v1/leases/1-1/runs",
            202,
            r#"{"run_id":"r-7","state":"queued"}"#,
        ),
        (
            "GET /v1/leases/1-1/runs/r-7",
            200,
            r#"{"run_id":"r-7","state":"exited","outcome":"out_of_memory","exit_code":null}"#,
        ),
        (
            "GET /v1/leases/1-1/runs/r-8",
            200,
            r#"{"run_id":"r-8","state":"running"}"#,
        ),
        ("DELETE /v1/leases/1-1", 200, r#"{"state":"released"}"#),
        ("DELETE /v1/leases/1-2", 202, r#"{"state":"releasing"}"#),
    ]);
    let (_dir, farm) = farm_at(&server.url);
    assert_eq!(farm.start_run("1-1", &run_ask()).unwrap(), "r-7");
    let sent = server.seen().remove(0);
    let body = sent.split_once("runs ").unwrap().1;
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["image"], "img");
    assert_eq!(body["limits"]["cpu_millis"], 4_000);
    assert_eq!(body["limits"]["wall_ms"], 90_000);
    assert_eq!(body["profile"], "prof");
    let over = farm.run("1-1", "r-7").unwrap();
    assert_eq!(
        (over.state.as_str(), over.outcome.as_deref()),
        ("exited", Some("out_of_memory"))
    );
    let live = farm.run("1-1", "r-8").unwrap();
    assert_eq!((live.state.as_str(), live.outcome), ("running", None));
    farm.release("1-1").unwrap();
    farm.release("1-2").unwrap();
    assert_eq!(
        farm.release("1-3").unwrap_err(),
        FarmError::Refused {
            status: 404,
            code: "not-found".to_owned()
        }
    );
}

/// Asserts the reads of the server's data: a host's shape from the host list (and an
/// unlisted host as an unreadable answer), the usage rows of a range from the query it
/// sends, and a curve with the minutes it has, with none when the key is unknown (404) or
/// has no fit yet.
#[test]
fn hosts_usage_and_curves_are_read() {
    let server = Canned::serve(vec![
        (
            "GET /v1/hosts",
            200,
            r#"{"hosts":[{"id":"h1","source":"registered","shape":{"cpu_millis":32000,"memory_bytes":1000,"gpus":0},"memory":{}}]}"#,
        ),
        (
            "GET /v1/usage/nodes/h1?from=10&to=11",
            200,
            r#"{"host":"h1","from":10,"to":11,"rows":[{"minute":10,"cpu_millis":8000,"ws_bytes":5,"leases":1}],"next":null}"#,
        ),
        (
            "GET /v1/usage/keys/known",
            200,
            r#"{"key":"known","runs":4,"curve":[{"minute":0,"cores_milli":500,"ws_bytes":7},{"minute":1,"cores_milli":null,"ws_bytes":9}],"last_used_day":1,"stale":false}"#,
        ),
        (
            "GET /v1/usage/keys/fresh",
            200,
            r#"{"key":"fresh","runs":1,"curve":null,"last_used_day":1,"stale":false}"#,
        ),
    ]);
    let (_dir, farm) = farm_at(&server.url);
    assert_eq!(
        farm.host("h1").unwrap(),
        HostShape {
            cpu_millis: 32_000,
            memory_bytes: 1_000
        }
    );
    assert!(matches!(farm.host("h9"), Err(FarmError::Malformed(_))));
    let rows = farm.usage_rows("h1", 10, 11).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].minute,
            rows[0].cpu_millis,
            rows[0].ws_bytes,
            rows[0].leases
        ),
        (10, 8_000, 5, 1)
    );
    assert_eq!(
        farm.curve("known").unwrap(),
        Some(Curve {
            runs: 4,
            ws_bytes: vec![7, 9],
            cores_milli: vec![500]
        })
    );
    assert_eq!(farm.curve("fresh").unwrap(), None);
    assert_eq!(farm.curve("unknown").unwrap(), None);
}

/// Asserts that an answer the harness cannot read is an error of its own kind for every
/// call, an error status with no body keeps its status and an empty code, and a server
/// that is not there is a transport error.
#[test]
fn unreadable_answers_and_a_missing_server_are_errors() {
    let junk = Canned::serve(vec![
        ("POST /v1/leases", 201, r#"{"nope":1}"#),
        ("GET /v1/leases", 200, r#"{"nope":1}"#),
        ("POST /v1/leases/1/runs", 202, r#"{"nope":1}"#),
        ("GET /v1/leases/1/runs/r", 200, r#"{"nope":1}"#),
        ("GET /v1/hosts", 200, r#"{"nope":1}"#),
        ("GET /v1/usage/nodes/h?from=1&to=2", 200, r#"{"nope":1}"#),
        ("GET /v1/usage/keys/k", 200, r#"{"nope":1}"#),
        ("DELETE /v1/leases/1", 500, ""),
    ]);
    let (_dir, farm) = farm_at(&junk.url);
    let malformed = |result: Result<(), FarmError>| matches!(result, Err(FarmError::Malformed(_)));
    assert!(malformed(farm.acquire(&lease_ask()).map(drop)));
    assert!(malformed(farm.lease("1").map(drop)));
    assert!(malformed(farm.start_run("1", &run_ask()).map(drop)));
    assert!(malformed(farm.run("1", "r").map(drop)));
    assert!(malformed(farm.host("h").map(drop)));
    assert!(malformed(farm.usage_rows("h", 1, 2).map(drop)));
    assert!(malformed(farm.curve("k").map(drop)));
    assert_eq!(
        farm.release("1").unwrap_err(),
        FarmError::Refused {
            status: 500,
            code: String::new()
        }
    );

    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (_dir, farm) = farm_at(&closed);
    assert!(matches!(farm.host("h"), Err(FarmError::Transport(_))));
    assert!(matches!(farm.lease("1"), Err(FarmError::Transport(_))));
    assert!(matches!(farm.release("1"), Err(FarmError::Transport(_))));
    assert!(matches!(
        farm.acquire(&lease_ask()),
        Err(FarmError::Transport(_))
    ));
}

/// Asserts that `connect` refuses what the client refuses: a plain-http server off the
/// loopback, and a token file that anyone can read.
#[test]
fn connect_refuses_unusable_flags() {
    let (dir, path) = token();
    assert!(HttpFarm::connect("http://example.com", &path, None).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(HttpFarm::connect("http://127.0.0.1:1", &path, None).is_err());
    drop(dir);
}
