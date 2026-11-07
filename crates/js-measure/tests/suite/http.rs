//! The live farm against a server that gives canned answers: the job requests it sends, the
//! answers it reads, the refusals and the answers it cannot read.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use js_measure::farm::Phase;
use js_measure::{Farm, FarmError, HostShape, HttpFarm, JobAsk, LogTail};
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
    // Status 999 stands for a 200 whose body stops short of its Content-Length.
    let (status, declared) = match status {
        999 => (200, text.len() + 50),
        other => (other, text.len()),
    };
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{text}"
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

/// A farm whose job server and main server are the same canned server.
fn farm_at(url: &str) -> (TestDir, HttpFarm) {
    two_farms(url, url)
}

/// A farm whose jobs go to `job_url` and whose host and usage reads go to `url`.
fn two_farms(url: &str, job_url: &str) -> (TestDir, HttpFarm) {
    let (dir, path) = token();
    (dir, HttpFarm::connect(url, job_url, &path, None).unwrap())
}

fn shared_ask() -> JobAsk {
    job("a", 90_000).ask
}

fn whole_ask() -> JobAsk {
    JobAsk {
        whole_host: true,
        ..shared_ask()
    }
}

/// Asserts the submit: the body a job sends (image, args, the CPU and memory it needs and
/// its wall limit in seconds, 64-bit numbers as text as the gateway writes them), the
/// whole-host job sending only `wholeHost`, the id read from the 201, and a refusal as the
/// server's status and code.
#[test]
fn a_job_is_submitted_with_its_needs() {
    let server = Canned::serve(vec![(
        "POST /v1/jobs",
        201,
        r#"{"job":{"id":"j-7","state":"JOB_STATE_QUEUED","spec":{}}}"#,
    )]);
    let (_dir, farm) = farm_at(&server.url);
    let mut ask = shared_ask();
    ask.args = vec!["--flag".to_owned()];
    assert_eq!(farm.submit(&ask).unwrap(), "j-7");
    assert_eq!(farm.submit(&whole_ask()).unwrap(), "j-7");
    let sent = server.seen();
    assert_eq!(sent.len(), 2);
    let body = |line: &str| -> serde_json::Value {
        serde_json::from_str(line.strip_prefix("POST /v1/jobs ").unwrap()).unwrap()
    };
    let spec = &body(&sent[0])["spec"];
    assert_eq!(spec["image"], "img");
    assert_eq!(spec["args"], serde_json::json!(["--flag"]));
    assert_eq!(
        spec["needs"],
        serde_json::json!({"cpuMillis": "4000", "memoryBytes": (8u64 << 30).to_string()})
    );
    assert_eq!(spec["wallLimitSeconds"], "90");
    assert_eq!(
        body(&sent[1])["spec"]["needs"],
        serde_json::json!({"wholeHost": true})
    );

    let full = Canned::serve(vec![(
        "POST /v1/jobs",
        422,
        r#"{"error":"no-capable-host"}"#,
    )]);
    let (_dir, farm) = farm_at(&full.url);
    assert_eq!(
        farm.submit(&shared_ask()).unwrap_err(),
        FarmError::Refused {
            status: 422,
            code: "no-capable-host".to_owned()
        }
    );
}

/// Asserts how a job that has not ended is read: queued with no host, placed on a host
/// and running, and that a job in a live state asks for no result.
#[test]
fn a_job_that_has_not_ended_is_read_by_state_and_host() {
    let server = Canned::serve(vec![
        (
            "GET /v1/jobs/q",
            200,
            r#"{"job":{"id":"q","state":"JOB_STATE_QUEUED"}}"#,
        ),
        (
            "GET /v1/jobs/p",
            200,
            r#"{"job":{"id":"p","state":"JOB_STATE_PLACED","hostId":"h1"}}"#,
        ),
        (
            "GET /v1/jobs/r",
            200,
            r#"{"job":{"id":"r","state":"JOB_STATE_RUNNING","hostId":"h1"}}"#,
        ),
    ]);
    let (_dir, farm) = farm_at(&server.url);
    let queued = farm.job("q").unwrap();
    assert_eq!(
        (queued.phase(), queued.host, queued.outcome),
        (Phase::Waiting, None, None)
    );
    let placed = farm.job("p").unwrap();
    assert_eq!(
        (placed.phase(), placed.host.as_deref()),
        (Phase::Waiting, Some("h1"))
    );
    let running = farm.job("r").unwrap();
    assert_eq!((running.phase(), running.log_tail), (Phase::Running, None));
    assert!(server.seen().iter().all(|line| !line.contains("/result")));
}

/// Asserts how an ended job is read: its outcome from the state and the reason of its
/// result (succeeded, out of memory, wall limit, any other failure, cancelled, lost, failed
/// to start), and its output tails decoded from base64 with `truncated`.
#[test]
fn an_ended_job_is_read_with_its_outcome_and_output() {
    let ended = |state: &str| {
        let body = format!(r#"{{"job":{{"id":"x","state":"{state}","hostId":"h1"}}}}"#);
        &*Box::leak(body.into_boxed_str())
    };
    let result = |reason: &str| {
        let body = format!(
            r#"{{"result":{{"reason":"{reason}","stdoutTail":"anMtbWVhc3VyZS1yZXN1bHQgeyJhIjoxfQo=","stderrTail":"d2Fybgo=","truncated":true}}}}"#
        );
        &*Box::leak(body.into_boxed_str())
    };
    let cases = [
        ("succeeded", "JOB_STATE_SUCCEEDED", "RESULT_REASON_EXIT"),
        ("out_of_memory", "JOB_STATE_FAILED", "RESULT_REASON_MEMORY"),
        ("wall_limit", "JOB_STATE_FAILED", "RESULT_REASON_WALL_LIMIT"),
        ("failed", "JOB_STATE_FAILED", "RESULT_REASON_EXIT"),
        (
            "cancelled",
            "JOB_STATE_CANCELLED",
            "RESULT_REASON_CANCELLED",
        ),
        ("lost", "JOB_STATE_LOST", "RESULT_REASON_UNSPECIFIED"),
        (
            "failed_to_start",
            "JOB_STATE_FAILED_TO_START",
            "RESULT_REASON_UNSPECIFIED",
        ),
    ];
    let routes: Vec<(&'static str, u16, &'static str)> = cases
        .iter()
        .flat_map(|(outcome, state, reason)| {
            let get: &'static str = Box::leak(format!("GET /v1/jobs/{outcome}").into_boxed_str());
            let got: &'static str =
                Box::leak(format!("GET /v1/jobs/{outcome}/result").into_boxed_str());
            [(get, 200, ended(state)), (got, 200, result(reason))]
        })
        .collect();
    let server = Canned::serve(routes);
    let (_dir, farm) = farm_at(&server.url);
    for (outcome, state, _) in cases {
        let standing = farm.job(outcome).unwrap();
        assert_eq!(standing.state, state);
        assert_eq!(standing.phase(), Phase::Over, "{outcome}");
        assert_eq!(standing.outcome.as_deref(), Some(outcome));
        assert_eq!(standing.host.as_deref(), Some("h1"));
        assert_eq!(
            standing.log_tail,
            Some(LogTail {
                stdout: "js-measure-result {\"a\":1}\n".to_owned(),
                stderr: "warn\n".to_owned(),
                truncated: true,
            })
        );
    }
}

/// Asserts that a result with no tails reads as empty text (not an error), that a tail
/// that is not base64 is an unreadable answer, and that a result the server refuses is the
/// server's refusal.
#[test]
fn a_result_without_tails_is_empty_and_a_bad_tail_is_unreadable() {
    let server = Canned::serve(vec![
        (
            "GET /v1/jobs/bare",
            200,
            r#"{"job":{"id":"bare","state":"JOB_STATE_SUCCEEDED"}}"#,
        ),
        ("GET /v1/jobs/bare/result", 200, r#"{"result":{}}"#),
        (
            "GET /v1/jobs/junk",
            200,
            r#"{"job":{"id":"junk","state":"JOB_STATE_SUCCEEDED"}}"#,
        ),
        (
            "GET /v1/jobs/junk/result",
            200,
            r#"{"result":{"stdoutTail":"not base64!"}}"#,
        ),
        (
            "GET /v1/jobs/gone",
            200,
            r#"{"job":{"id":"gone","state":"JOB_STATE_FAILED"}}"#,
        ),
        ("GET /v1/jobs/gone/result", 410, r#"{"error":"expired"}"#),
    ]);
    let (_dir, farm) = farm_at(&server.url);
    let bare = farm.job("bare").unwrap();
    assert_eq!(bare.host, None);
    assert_eq!(bare.outcome.as_deref(), Some("succeeded"));
    assert_eq!(bare.log_tail, Some(LogTail::default()));
    assert!(matches!(farm.job("junk"), Err(FarmError::Malformed(_))));
    assert_eq!(
        farm.job("gone").unwrap_err(),
        FarmError::Refused {
            status: 410,
            code: "expired".to_owned()
        }
    );
}

/// Asserts the cancel: it is a POST of `:cancel` on the job, accepted on 200, and a
/// refusal (an unknown job) is the server's status and code.
#[test]
fn a_job_is_cancelled() {
    let server = Canned::serve(vec![
        ("POST /v1/jobs/j-1:cancel", 200, r#"{"job":{"id":"j-1"}}"#),
        (
            "POST /v1/jobs/j-2:cancel",
            404,
            r#"{"error":"unknown-job"}"#,
        ),
    ]);
    let (_dir, farm) = farm_at(&server.url);
    farm.cancel("j-1").unwrap();
    assert_eq!(
        farm.cancel("j-2").unwrap_err(),
        FarmError::Refused {
            status: 404,
            code: "unknown-job".to_owned()
        }
    );
}

/// Asserts that the jobs go to the job server and the host and usage reads to the main
/// server: each canned server sees only its own requests.
#[test]
fn jobs_and_reads_go_to_their_own_servers() {
    let main = Canned::serve(vec![(
        "GET /v1/hosts",
        200,
        r#"{"hosts":[{"id":"h1","shape":{"cpu_millis":1,"memory_bytes":2}}]}"#,
    )]);
    let jobs = Canned::serve(vec![("POST /v1/jobs", 201, r#"{"job":{"id":"j-1"}}"#)]);
    let (_dir, farm) = two_farms(&main.url, &jobs.url);
    farm.submit(&shared_ask()).unwrap();
    farm.host("h1").unwrap();
    assert_eq!(main.seen(), vec!["GET /v1/hosts ".to_owned()]);
    assert_eq!(jobs.seen().len(), 1);
    assert!(jobs.seen()[0].starts_with("POST /v1/jobs "));
}

/// Asserts the reads of the server's data: a host's shape from the host list (and an
/// unlisted host as an unreadable answer) and the usage rows of a range from the query it
/// sends.
#[test]
fn hosts_and_usage_are_read() {
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
}

/// Asserts that an answer the harness cannot read is an error of its own kind for every
/// call, an error status with no body keeps its status and an empty code, and a server
/// that is not there is a transport error.
#[test]
fn unreadable_answers_and_a_missing_server_are_errors() {
    let junk = Canned::serve(vec![
        ("POST /v1/jobs", 201, r#"{"nope":1}"#),
        ("GET /v1/jobs/j", 200, r#"{"nope":1}"#),
        ("GET /v1/hosts", 200, r#"{"nope":1}"#),
        ("GET /v1/usage/nodes/h?from=1&to=2", 200, r#"{"nope":1}"#),
        ("POST /v1/jobs/j:cancel", 500, ""),
        (
            "GET /v1/jobs/k",
            200,
            r#"{"job":{"id":"k","state":"JOB_STATE_FAILED"}}"#,
        ),
        ("GET /v1/jobs/k/result", 200, r#"{"nope":1}"#),
    ]);
    let (_dir, farm) = farm_at(&junk.url);
    let malformed = |result: Result<(), FarmError>| matches!(result, Err(FarmError::Malformed(_)));
    assert!(malformed(farm.submit(&shared_ask()).map(drop)));
    assert!(malformed(farm.job("j").map(drop)));
    assert!(malformed(farm.job("k").map(drop)));
    assert!(malformed(farm.host("h").map(drop)));
    assert!(malformed(farm.usage_rows("h", 1, 2).map(drop)));
    assert_eq!(
        farm.cancel("j").unwrap_err(),
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
    assert!(matches!(
        farm.usage_rows("h", 1, 2),
        Err(FarmError::Transport(_))
    ));
    assert!(matches!(farm.job("j"), Err(FarmError::Transport(_))));
    assert!(matches!(farm.cancel("j"), Err(FarmError::Transport(_))));
    assert!(matches!(
        farm.submit(&shared_ask()),
        Err(FarmError::Transport(_))
    ));
}

/// Asserts that `connect` refuses what the client refuses: a plain-http server off the
/// loopback (as the main server or as the job server), and a token file that anyone can
/// read.
#[test]
fn connect_refuses_unusable_flags() {
    let (dir, path) = token();
    let local = "http://127.0.0.1:1";
    assert!(HttpFarm::connect("http://example.com", local, &path, None).is_err());
    assert!(HttpFarm::connect(local, "http://example.com", &path, None).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(HttpFarm::connect(local, local, &path, None).is_err());
    drop(dir);
}

/// Asserts that a server that refuses the token (401, 403) is a refusal with its status
/// and code, for a read and for a write, and that a body that stops short of its declared
/// length is a transport error and not a half-read answer.
#[test]
fn refused_tokens_and_truncated_bodies() {
    let server = Canned::serve(vec![
        ("GET /v1/hosts", 401, r#"{"error":"unauthorized"}"#),
        ("POST /v1/jobs", 403, r#"{"error":"forbidden"}"#),
        ("GET /v1/jobs/j", 999, r#"{"job":{"#),
    ]);
    let (_dir, farm) = farm_at(&server.url);
    assert_eq!(
        farm.host("h1").unwrap_err(),
        FarmError::Refused {
            status: 401,
            code: "unauthorized".to_owned()
        }
    );
    assert_eq!(
        farm.submit(&shared_ask()).unwrap_err(),
        FarmError::Refused {
            status: 403,
            code: "forbidden".to_owned()
        }
    );
    assert!(matches!(farm.job("j"), Err(FarmError::Transport(_))));
}
