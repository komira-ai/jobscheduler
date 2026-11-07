//! The job front, driven through a scripted client port with no server: each answer of the
//! job service mapped to the one event its module doc says, the body of a submit, and what
//! a cancel and a give-up do at each stage.
//!
//! Sizes: no server and no agent; each test makes a few dozen scripted calls.

use std::collections::VecDeque;
use std::time::Duration;

use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, Response};
use js_sim_cell::server::CLIENT_TOKEN;
use js_sim_cell::{
    CallKind, ClientEvent, ClientPort, Front, HowEnded, ItemId, JobFront, SIM_RUN_IMAGE,
    ServerSpec, Shape, Work,
};
use js_types::sched::FarmTime;
use js_types::{HostId, Resources};
use serde_json::{Value, json};

/// How long each item's run lasts.
const RUN: Duration = Duration::from_secs(10);

/// The front's poll period.
const POLL_MS: u64 = 5_000;

/// The path of item 0's job.
const PATH: &str = "/v1/jobs/sim-job-0";

/// A client port that answers from a script and writes each call down.
struct Scripted {
    answers: VecDeque<(u16, Value)>,
    calls: Vec<String>,
    /// The body of each call as JSON (`Null` when it has none).
    bodies: Vec<Value>,
}

impl ClientPort for Scripted {
    fn call(&mut self, request: Request<Body>) -> Response<String> {
        let bearer = request.headers().get(AUTHORIZATION).cloned();
        assert_eq!(
            bearer.and_then(|value| value.to_str().ok().map(str::to_owned)),
            Some(format!("Bearer {CLIENT_TOKEN}"))
        );
        self.calls
            .push(format!("{} {}", request.method(), request.uri()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        let bytes = runtime
            .block_on(axum::body::to_bytes(request.into_body(), usize::MAX))
            .expect("a body");
        self.bodies
            .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
        let (status, body) = self.answers.pop_front().expect("an answer for every call");
        Response::builder()
            .status(status)
            .body(body.to_string())
            .expect("a response")
    }
}

/// Polls `front` at `at` (virtual milliseconds) with `answers` as the server's, which must
/// all be taken; returns the events heard, the calls made and their bodies.
fn poll(
    front: &mut JobFront,
    at: u64,
    answers: Vec<(u16, Value)>,
) -> (Vec<ClientEvent>, Vec<String>, Vec<Value>) {
    let mut port = Scripted {
        answers: answers.into(),
        calls: Vec::new(),
        bodies: Vec::new(),
    };
    let heard = front.poll(FarmTime::from_millis(at), &mut port);
    assert!(
        port.answers.is_empty(),
        "unused answers: {:?}",
        port.answers
    );
    let heard = heard.into_iter().map(|(item, event)| {
        assert_eq!(item, ItemId(0));
        event
    });
    (heard.collect(), port.calls, port.bodies)
}

/// One item of `work` submitted at time zero, polled every [`POLL_MS`] with the answers of
/// each poll in turn: every event it heard and every call made, with the front.
fn one(work: &Work, polls: Vec<Vec<(u16, Value)>>) -> (Vec<ClientEvent>, Vec<String>, JobFront) {
    let mut front = JobFront::new();
    front.submit(ItemId(0), work, FarmTime::default());
    let mut events = Vec::new();
    let mut calls = Vec::new();
    for (n, answers) in (0..).zip(polls) {
        let (heard, made, _) = poll(&mut front, n * POLL_MS, answers);
        events.extend(heard);
        calls.extend(made);
    }
    (events, calls, front)
}

fn accepted() -> (u16, Value) {
    (
        201,
        json!({ "job": { "id": "sim-job-0", "state": "JOB_STATE_QUEUED" } }),
    )
}

/// GetJob's answer: the job in `state`, on `host` when given, started when `started`.
fn job(state: &str, host: Option<&str>, started: bool) -> (u16, Value) {
    let mut job = json!({ "id": "sim-job-0", "state": state, "times": { "queuedAtMs": "0" } });
    if let Some(host) = host {
        job["hostId"] = json!(host);
    }
    if started {
        job["times"]["startedAtMs"] = json!("4000");
    }
    (200, json!({ "job": job }))
}

fn status(code: u16) -> (u16, Value) {
    (code, json!({ "error": "code" }))
}

fn result() -> (u16, Value) {
    (
        200,
        json!({ "result": { "state": "JOB_STATE_SUCCEEDED", "exitCode": 0 } }),
    )
}

fn terminal(how: HowEnded) -> ClientEvent {
    ClientEvent::Terminal { how }
}

fn placed() -> ClientEvent {
    ClientEvent::Placed {
        host: HostId::new("h000").expect("a host id"),
    }
}

/// The events of one item answered `answer` at its first read, its result then read 200.
fn read_as(answer: (u16, Value)) -> Vec<ClientEvent> {
    let polls = vec![vec![accepted(), answer, result()]];
    let (events, _, front) = one(&Work::run_for(RUN), polls);
    assert_eq!(front.next_poll(), None, "the item is done");
    events
}

/// Asserts that each answer of the job service produces exactly the event the job front's
/// module doc says: a submit answered 201 is read in the same poll, 422 and 5xx are
/// submitted again, any other refusal fails the item; a read's `hostId` is `Placed` once,
/// `JOB_STATE_STARTING`, `JOB_STATE_RUNNING` or a start time is `Started` once, each
/// terminal state its own `HowEnded`, 404 `Lost`, and anything else read again; the result
/// read after a terminal answer is `Released` on 200, 404 or 410 and read again on anything
/// else; that the submit asks for the work's cpu and memory under the item's own job id,
/// with no GPU, the run's duration as the one argument and the give-up time as the wall
/// limit; and that every answer is kept under its call's kind. Catches a state mapped to the
/// wrong `HowEnded`, a second `Placed` or `Started`, a submit not sent again after the
/// service refused it for want of a host, and a job that asks for a GPU the server could
/// never start it with.
#[test]
fn the_job_front_maps_each_service_answer_to_one_event() {
    let work = Work::run_for(RUN);
    let polls = vec![
        vec![status(422)],
        vec![status(503)],
        vec![accepted(), job("JOB_STATE_QUEUED", None, false)],
        vec![job("JOB_STATE_PLACED", Some("h000"), false)],
        vec![job("JOB_STATE_STARTING", Some("h000"), false)],
        vec![status(500)],
        vec![job("JOB_STATE_RUNNING", Some("h000"), true)],
        vec![job("JOB_STATE_SUCCEEDED", Some("h000"), true), status(503)],
        vec![result()],
    ];
    let (events, calls, front) = one(&work, polls);
    let wanted = [
        placed(),
        ClientEvent::Started,
        terminal(HowEnded::Succeeded),
        ClientEvent::Released,
    ];
    assert_eq!(events, wanted);
    let read = format!("GET {PATH}");
    let result_read = format!("GET {PATH}/result");
    let mut wanted_calls = vec!["POST /v1/jobs".to_owned(); 3];
    wanted_calls.extend(vec![read; 6]);
    wanted_calls.extend(vec![result_read; 2]);
    assert_eq!(calls, wanted_calls);
    assert_eq!(front.next_poll(), None);
    let kinds: Vec<CallKind> = front.answers().iter().map(|answer| answer.kind).collect();
    let mut wanted_kinds = vec![CallKind::Queue; 3];
    wanted_kinds.extend(vec![CallKind::Read; 8]);
    assert_eq!(kinds, wanted_kinds);
    assert_eq!(front.answers()[2].status, 201);

    let mut front = JobFront::new();
    front.submit(ItemId(0), &work, FarmTime::default());
    let (_, _, bodies) = poll(&mut front, 0, vec![status(422)]);
    let resources = work.resources();
    let wanted_body = json!({
        "spec": {
            "jobId": "sim-job-0",
            "image": SIM_RUN_IMAGE,
            "args": ["10000"],
            "needs": {
                "cpuMillis": resources.cpu_millis().to_string(),
                "memoryBytes": resources.memory_bytes().to_string(),
            },
            "wallLimitSeconds": "3600",
        }
    });
    assert_eq!(bodies, [wanted_body]);
    assert_eq!(front.next_poll(), Some(FarmTime::from_millis(POLL_MS)));

    let started = || vec![placed(), ClientEvent::Started];
    let cases = [
        ("JOB_STATE_FAILED", HowEnded::Failed),
        ("JOB_STATE_CANCELLED", HowEnded::Cancelled),
        ("JOB_STATE_LOST", HowEnded::Lost),
    ];
    for (state, how) in cases {
        let mut wanted = started();
        wanted.extend([terminal(how), ClientEvent::Released]);
        assert_eq!(read_as(job(state, Some("h000"), true)), wanted, "{state}");
    }
    let never = job("JOB_STATE_FAILED_TO_START", None, false);
    assert_eq!(
        read_as(never),
        [terminal(HowEnded::Failed), ClientEvent::Released]
    );
    let polls = vec![vec![
        accepted(),
        status(404),
        (410, json!({ "error": "expired" })),
    ]];
    let (events, _, _) = one(&work, polls);
    assert_eq!(events, [terminal(HowEnded::Lost), ClientEvent::Released]);
    let polls = vec![vec![accepted(), status(404), status(404)]];
    let (events, _, _) = one(&work, polls);
    assert_eq!(events, [terminal(HowEnded::Lost), ClientEvent::Released]);

    let polls = vec![vec![(409, json!({ "error": "job-id-conflict" }))]];
    let (events, calls, front) = one(&work, polls);
    assert_eq!(events, [terminal(HowEnded::Failed)]);
    assert_eq!(calls, ["POST /v1/jobs"]);
    assert_eq!(front.next_poll(), None);
}

/// Asserts what a cancel and a give-up do at each stage, as the job front's module doc
/// says: a cancel before the job is accepted ends the item `Cancelled` with no call; a
/// cancel of an accepted job sends its cancel once and reads the job's end (`Cancelled`,
/// and a failed start `Cancelled` too); a give-up before the job is accepted ends the item
/// with no call, and one after sends the job's cancel until it is answered 200 or 404,
/// then `Released`; and a cancel of an item the front does not hold changes nothing.
/// Catches a cancel sent twice, a cancelled job reported failed, and a job left running
/// after its client gave up.
#[test]
fn the_job_front_cancels_and_gives_up_as_its_module_doc_says() {
    let cancel_call = format!("POST {PATH}:cancel");

    let mut front = JobFront::new();
    front.submit(ItemId(0), &Work::run_for(RUN), FarmTime::default());
    front.cancel(ItemId(0), FarmTime::default());
    front.cancel(ItemId(9), FarmTime::default());
    let (events, calls, _) = poll(&mut front, 0, Vec::new());
    assert_eq!(events, [terminal(HowEnded::Cancelled)]);
    assert!(calls.is_empty());
    assert_eq!(front.next_poll(), None);

    for (state, started) in [
        ("JOB_STATE_CANCELLED", true),
        ("JOB_STATE_FAILED_TO_START", false),
    ] {
        let mut front = JobFront::new();
        front.submit(ItemId(0), &Work::run_for(RUN), FarmTime::default());
        poll(
            &mut front,
            0,
            vec![accepted(), job("JOB_STATE_QUEUED", None, false)],
        );
        front.cancel(ItemId(0), FarmTime::from_millis(1_000));
        assert_eq!(front.next_poll(), Some(FarmTime::from_millis(1_000)));
        let running = job("JOB_STATE_RUNNING", Some("h000"), true);
        let (events, calls, bodies) = poll(&mut front, 1_000, vec![(200, json!({})), running]);
        assert_eq!(events, [placed(), ClientEvent::Started]);
        assert_eq!(calls[0], cancel_call);
        assert_eq!(bodies[0], json!({}));
        let ended = job(state, Some("h000"), started);
        let (events, calls, _) = poll(&mut front, POLL_MS, vec![ended, result()]);
        assert_eq!(
            events,
            [terminal(HowEnded::Cancelled), ClientEvent::Released],
            "{state}"
        );
        assert!(!calls.contains(&cancel_call), "the cancel is sent once");
        let kinds = front.answers().iter().map(|answer| answer.kind);
        assert_eq!(kinds.filter(|kind| *kind == CallKind::Cancel).count(), 1);
    }

    let soon = Work::run_for(RUN).give_up_after(Duration::from_secs(10));
    let polls = vec![vec![status(422)], vec![status(422)], vec![]];
    let (events, calls, front) = one(&soon, polls);
    assert_eq!(events, [terminal(HowEnded::GaveUp)]);
    assert_eq!(calls.len(), 2);
    assert_eq!(front.next_poll(), None);

    for gone in [200, 404] {
        let polls = vec![
            vec![accepted(), job("JOB_STATE_RUNNING", Some("h000"), true)],
            vec![job("JOB_STATE_RUNNING", Some("h000"), true)],
            vec![status(503)],
            vec![(gone, json!({}))],
        ];
        let (events, calls, front) = one(&soon, polls);
        let wanted = [
            placed(),
            ClientEvent::Started,
            terminal(HowEnded::GaveUp),
            ClientEvent::Released,
        ];
        assert_eq!(events, wanted);
        let cancels = calls.iter().filter(|call| **call == cancel_call).count();
        assert_eq!(cancels, 2);
        assert_eq!(front.next_poll(), None);
    }

    let whole =
        JobFront::factory().make(&ServerSpec::registered_pool("burst", Shape::gpu_host(), 1));
    assert_eq!(whole.next_poll(), None);
    let mut front = JobFront::new();
    front.submit(ItemId(0), &Work::whole_host(RUN), FarmTime::default());
    let (_, _, bodies) = poll(&mut front, 0, vec![status(422)]);
    assert_eq!(bodies[0]["spec"]["needs"], json!({ "wholeHost": true }));
}

/// Asserts that the job front sends an item's own resources: the submit asks for the work's
/// cpu and memory (as decimal strings, and no GPU) with its give-up time in whole seconds
/// as the wall limit, and runs `sim-run` for the work's duration in milliseconds under the
/// item's own job id. Catches a front that sends the default share, or the default give-up
/// time, in place of what the work asks for.
#[test]
fn the_job_front_sends_the_works_own_resources_in_the_submit() {
    let needs = Resources::new(500, 512, 2, Some("nvidia-h100".to_owned())).expect("resources");
    let work = Work::run_for(RUN)
        .needs(needs)
        .give_up_after(Duration::from_secs(900));
    let mut front = JobFront::new();
    front.submit(ItemId(0), &work, FarmTime::default());
    let (_, calls, bodies) = poll(&mut front, 0, vec![status(422)]);
    assert_eq!(calls, ["POST /v1/jobs"]);
    let spec = &bodies[0]["spec"];
    assert_eq!(spec["jobId"], "sim-job-0", "{spec}");
    assert_eq!(spec["image"], SIM_RUN_IMAGE, "{spec}");
    assert_eq!(spec["args"], json!([RUN.as_millis().to_string()]), "{spec}");
    let needs = json!({ "cpuMillis": "500", "memoryBytes": "512" });
    assert_eq!(spec["needs"], needs, "{spec}");
    assert_eq!(spec["wallLimitSeconds"], "900", "{spec}");
}

/// Asserts that each kind of call a front keeps an answer for has its own stable name:
/// `queue` for the submit, `read` for a read of the job or its result, `cancel` for the
/// stop. Catches two kinds that share a name, which a report of the answers would then
/// run together.
#[test]
fn each_kind_of_call_has_its_own_name() {
    let kinds = [CallKind::Queue, CallKind::Read, CallKind::Cancel];
    let names: Vec<&str> = kinds.into_iter().map(CallKind::name).collect();
    assert_eq!(names, ["queue", "read", "cancel"]);
}
