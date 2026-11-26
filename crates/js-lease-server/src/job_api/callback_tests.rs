//! The job service with its submit taking completion callbacks, as a test builds it until the
//! release that accepts them (`docs/design/job-retry-and-completion-callback.md`, sections 5.2,
//! 5.5 and 7): the submit's checks of a callback, the status `GetJob` and `ListJobs` show, and
//! the signing key kept out of every answer, row, count and log line. Deliveries run on the
//! test's thread against loopback receivers (`crate::callbacks::receiver`).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use js_agent_wire::Outcome;
use js_callback::AddressRule;
use js_jobs::{AllocationStep, InstanceId, JobId, Retention};
use js_proto::refusal::refusal_of;
use js_proto::v1::{
    self, GetJobRequest, GetJobResponse, ListJobsRequest, ListJobsResponse, SubmitJobRequest,
    SubmitJobResponse,
};
use js_state::{ManualClock, Store};
use js_types::HostId;
use serde_json::{Value, json};
use tonic::Code;
use tonic_prost::prost::Message;

use super::JobApi;
use super::read_tests::json_of;
use super::submit::{Admission, Ctx};
use super::tests::{TOKEN, token};
use super::writes_rig::{CEILING, Fixed, NEVER, Via, Writes, rows, to_json, wire};
use crate::agent_api::job_rig::{BOOKED, FakeAgent, OneClock, START_MS, clocked_rig, tables_on};
use crate::api::rig::{Lines, process_sink};
use crate::callbacks::Delivery;
use crate::callbacks::receiver::{Authority, Loopback, Receiver, status};
use crate::clock::SharedClock;
use crate::job_store::ServerJobs;
use crate::metrics::{Meter, NoMetrics, Recorder, rendered};

/// The key the redaction test's job is signed with: 40 bytes no answer, row or line may hold.
const KEY: &[u8; 40] = b"never-shown-callback-signing-key-0123456";

/// The job service over `jobs` and `clock`, admitting every job, its submit taking callbacks
/// judged by `rule`.
async fn accepting(jobs: Arc<ServerJobs>, clock: SharedClock, rule: AddressRule) -> Writes {
    let admission: Arc<dyn Admission> = Fixed::new(true);
    let submit = Ctx::served(CEILING, admission).accepting_callbacks(rule);
    Writes::over(JobApi::new(jobs, clock).with_submit(submit), token()).await
}

/// Tables of a fake store on a manual clock, with [`accepting`] over them.
async fn over_fake(rule: AddressRule) -> (Arc<ManualClock>, Writes) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    (clock, accepting(Arc::new(jobs), shared, rule).await)
}

/// Delivery from `jobs` as `server-a` through a sender `authority` trusts alone, its answers
/// counted by `recorder`.
fn delivery(jobs: &Arc<ServerJobs>, authority: &Authority, recorder: &Arc<Recorder>) -> Delivery {
    let meter = Meter::new(recorder.clone(), Arc::new(NoMetrics));
    let sender = authority.sender(AddressRule::new([]), Loopback::new());
    Delivery::new(Arc::clone(jobs), meter, sender, InstanceId::new("server-a"))
}

/// A callback to `url` signed with `key`, tried for `deadline` seconds.
fn callback(url: &str, key: &[u8], deadline: Option<u32>) -> v1::Callback {
    v1::Callback {
        url: url.to_owned(),
        signing_key: key.to_vec(),
        deadline_seconds: deadline,
    }
}

/// `callback` as every answer shows it: its URL and deadline, and no key.
fn shown(callback: &v1::Callback) -> v1::Callback {
    v1::Callback {
        signing_key: Vec::new(),
        ..callback.clone()
    }
}

/// The 400 body the gateway answers for `field` refused with `code`.
fn refused_body(field: &str, code: &str) -> Value {
    json!({
        "error": "invalid-argument",
        "field": format!("{field}: {code}"),
        "reason": "the service refused an argument of the request",
    })
}

/// Asserts that each callback the submit's checks refuse is answered `INVALID_ARGUMENT` with
/// exactly `field: <field>: <code>`, no `Refusal` detail, and over JSON exactly the 400 body
/// naming them, and that nothing is recorded: a URL that is plain http, has userinfo, a
/// fragment, no host, a port of 0, a space, or one byte too many; a host literal the address
/// rule denies (loopback in each spelling, an IPv4-mapped loopback, link-local metadata, the
/// providers' metadata addresses, carrier-grade NAT, and an address of the cluster's own); a
/// key of 0, 31 and 65 bytes; and an empty callback, whose URL is the first fault. Catches a
/// literal inward address recorded and tried, a key of the wrong length recorded, and a
/// refusal that repeats the URL or the key.
#[tokio::test]
async fn submit_refuses_each_bad_callback() {
    let cluster: IpAddr = [10, 1, 2, 3].into();
    let (_, writes) = over_fake(AddressRule::new([cluster])).await;
    let key = [7_u8; 32];
    let url = |host: &str| format!("https://{host}/js-done/receiver-ref?token=receiver-secret");
    let longest = format!("https://h.example/{}", "p".repeat(2048 - 18));
    let url_fault = |text: String, code| (callback(&text, &key, None), "spec.callback.url", code);
    let mut cases = vec![
        url_fault(url("hooks.example").replace("https", "http"), "not-https"),
        url_fault(url("user:pass@hooks.example"), "userinfo"),
        url_fault(format!("{}#frag", url("hooks.example")), "fragment"),
        url_fault("https:///x".to_owned(), "no-host"),
        url_fault(url("hooks.example:0"), "bad-port"),
        url_fault(format!("{} x", url("hooks.example")), "not-printable-ascii"),
        url_fault(format!("{longest}p"), "too-long"),
    ];
    for host in [
        "127.0.0.1",
        "[::1]",
        "2130706433",
        "0x7f.1",
        "[::ffff:127.0.0.1]",
        "169.254.169.254",
        "168.63.129.16",
        "[fd00:ec2::254]",
        "100.100.100.200",
        "10.1.2.3",
    ] {
        cases.push(url_fault(url(host), "refused-address"));
    }
    for len in [0, 31, 65] {
        let given = callback(&url("hooks.example"), &vec![1; len], None);
        cases.push((given, "spec.callback.signingKey", "bad-length"));
    }
    cases.push((v1::Callback::default(), "spec.callback.url", "not-https"));
    for (index, (given, field, code)) in cases.into_iter().enumerate() {
        let spec = v1::JobSpec {
            callback: Some(given),
            ..wire(&format!("bad-{index}"))
        };
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        let message = format!("field: {field}: {code}");
        assert_eq!(
            (status.code(), status.message()),
            (Code::InvalidArgument, message.as_str()),
            "{index}"
        );
        assert_eq!(refusal_of(&status), None, "{index}");
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body),
            (StatusCode::BAD_REQUEST, refused_body(field, code)),
            "{index}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}

/// Asserts that each callback the checks admit is recorded, and the answer shows it with its
/// URL and deadline as given and an empty key, and its status armed (nothing tried, no
/// deadline yet), over both transports: a name, a private and a public address literal (IPv4
/// and IPv6), keys of 32 and 64 bytes, a deadline of 0 and none, and a URL of exactly 2048
/// bytes. Catches a private address refused (receivers live on them), a bound off by one, and
/// a key echoed.
#[tokio::test]
async fn submit_takes_each_good_callback_and_shows_it_armed() {
    let (_, writes) = over_fake(AddressRule::new([])).await;
    let longest = format!("https://h.example/{}", "p".repeat(2048 - 18));
    let cases = [
        callback("https://hooks.example/js-done/7", &[1; 32], None),
        callback("https://10.0.0.5:8443/x", &[2; 64], Some(0)),
        callback("https://93.184.216.34/x", &[3; 40], Some(60)),
        callback("https://[2606:4700::1111]/x", &[4; 33], None),
        callback(&longest, &[5; 63], Some(86_400)),
    ];
    let armed = v1::CallbackStatus {
        state: v1::CallbackState::Armed.into(),
        ..v1::CallbackStatus::default()
    };
    for (index, given) in cases.iter().enumerate() {
        for via in Via::BOTH {
            let spec = v1::JobSpec {
                callback: Some(given.clone()),
                ..wire(&format!("good-{index}"))
            };
            let job = writes.submit(via, &spec).await.expect("recorded");
            let echoed = job.spec.and_then(|spec| spec.callback);
            assert_eq!(echoed, Some(shown(given)), "{index} {via:?}");
            assert_eq!(job.callback_status, Some(armed.clone()), "{index} {via:?}");
        }
    }
    assert_eq!(rows(&writes.jobs), cases.len());
}

/// The job `id` as `GetJob` answers it over gRPC.
async fn get(writes: &Writes, id: &str) -> v1::Job {
    let request = GetJobRequest {
        job_id: id.to_owned(),
    };
    let answer: GetJobResponse = writes.grpc("GetJob", request).await.expect("read");
    answer.job.expect("a job")
}

/// Every job `ListJobs` answers over gRPC, by id.
async fn listed(writes: &Writes) -> Vec<(String, Option<v1::CallbackStatus>)> {
    let request = ListJobsRequest::default();
    let answer: ListJobsResponse = writes.grpc("ListJobs", request).await.expect("read");
    let jobs = answer.jobs.into_iter();
    jobs.map(|job| (job.id, job.callback_status)).collect()
}

/// Asserts that `GetJob` and `ListJobs` show where each job's callback delivery stands, as its
/// row says: none for a job without a callback; armed with nothing tried and no deadline for a
/// queued one; once ended, its deadline the end plus the deadline asked for, and pending with
/// one try and the class `failed` after a 500; delivered with one try and its delivery time
/// after a 2xx; abandoned after a 410; expired with no try when its deadline (0) was its end;
/// and the JSON gateway the same status. Catches a status read from the wrong row, a state
/// mapped to another, a try or delivery time other than the row's, a deadline that is not the
/// end plus the deadline asked for, and a
/// listing that shows statuses its page did not read.
#[tokio::test]
async fn get_job_shows_the_callback_status() {
    let (_, writes) = over_fake(AddressRule::new([])).await;
    let authority = Authority::new();
    let recorder = Arc::new(Recorder::default());
    let deliveries = delivery(&writes.jobs, &authority, &recorder);
    let plain = writes
        .submit_grpc(&wire("cs-0-plain"))
        .await
        .expect("recorded");
    assert_eq!(plain.callback_status, None);
    let mut receivers = Vec::new();
    for (id, reply, deadline) in [
        ("cs-1-armed", 200, Some(3600)),
        ("cs-2-failed", 500, Some(3600)),
        ("cs-3-delivered", 200, Some(3600)),
        ("cs-4-abandoned", 410, Some(3600)),
        ("cs-5-expired", 200, Some(0)),
    ] {
        let receiver = Receiver::start(&authority, &[status(reply)]);
        let given = callback(&receiver.url("/done"), &[9; 32], deadline);
        let spec = v1::JobSpec {
            callback: Some(given),
            ..wire(id)
        };
        writes.submit_grpc(&spec).await.expect("recorded");
        receivers.push(receiver);
    }
    for id in [
        "cs-2-failed",
        "cs-3-delivered",
        "cs-4-abandoned",
        "cs-5-expired",
    ] {
        writes.cancel_grpc(id).await.expect("cancelled");
    }
    while deliveries.pass() {}
    let ended_at = |job: &v1::Job| job.times.as_ref().and_then(|times| times.ended_at_ms);
    let deadline = |job: &v1::Job, secs: u64| ended_at(job).map(|at| at + secs * 1000);
    let mut expected = vec![("cs-0-plain".to_owned(), None)];
    let armed = get(&writes, "cs-1-armed").await;
    let status = v1::CallbackStatus {
        state: v1::CallbackState::Armed.into(),
        ..v1::CallbackStatus::default()
    };
    assert_eq!(armed.callback_status, Some(status.clone()));
    expected.push((armed.id, Some(status)));
    let millis = |at: Option<js_state::StoreTime>| {
        at.map(|at| u64::try_from(at.as_duration().as_millis()).expect("fits"))
    };
    for (id, state, tries, failure, secs) in [
        ("cs-2-failed", v1::CallbackState::Pending, 1, "failed", 3600),
        ("cs-3-delivered", v1::CallbackState::Delivered, 1, "", 3600),
        ("cs-4-abandoned", v1::CallbackState::Abandoned, 1, "", 3600),
        ("cs-5-expired", v1::CallbackState::Expired, 0, "", 0),
    ] {
        let job = get(&writes, id).await;
        let shown = job.callback_status.clone().expect(id);
        let row = writes.jobs.callback_status(&JobId::parse(id).expect(id));
        let row = row.expect("reads").expect("a row");
        let tried = millis(row.last_try_at);
        let delivered = millis(row.delivered_at);
        let status = v1::CallbackStatus {
            state: state.into(),
            tries,
            last_try_at_ms: tried,
            last_failure: failure.to_owned(),
            delivered_at_ms: delivered,
            deadline_at_ms: deadline(&job, secs),
        };
        assert_eq!(shown, status, "{id}");
        assert_eq!(tried.is_some(), tries > 0, "{id}: tried at");
        let delivered_only = state == v1::CallbackState::Delivered;
        assert_eq!(delivered.is_some(), delivered_only, "{id}");
        expected.push((job.id.clone(), Some(status)));
    }
    assert_eq!(listed(&writes).await, expected);
    let (http, body) = writes
        .http_as(TOKEN, "GET", "/v1/jobs/cs-3-delivered", None)
        .await;
    assert_eq!(http, StatusCode::OK);
    let status = expected[3].1.clone().expect("a status");
    let name = "CallbackStatus";
    assert_eq!(
        json_of(&body)["job"]["callbackStatus"],
        to_json(name, &status)
    );
}

/// Captures the log lines of this thread at INFO and above, without times, until the guard is
/// dropped.
fn capture() -> (Lines, tracing::subscriber::DefaultGuard) {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (lines, tracing::subscriber::set_default(subscriber))
}

/// Whether `bytes` holds [`KEY`], as its bytes or in any base64 spelling.
fn holds_key(bytes: &[u8]) -> bool {
    let spellings = [
        KEY.to_vec(),
        STANDARD.encode(KEY).into_bytes(),
        URL_SAFE.encode(KEY).into_bytes(),
        URL_SAFE_NO_PAD.encode(KEY).into_bytes(),
    ];
    let within = |part: &[u8]| bytes.windows(part.len()).any(|window| window == part);
    spellings.iter().any(|spelling| within(spelling))
}

/// The keys of every row of `store` that holds [`KEY`].
fn rows_holding_key(store: &dyn Store) -> Vec<String> {
    let rows = store.scan_prefix("").expect("reads");
    let holding = rows.into_iter().filter(|(_, row)| holds_key(&row.value));
    holding.map(|(key, _)| key).collect()
}

/// Asserts that a job's signing key leaves the server nowhere, from its submit over both
/// transports to its delivery: the `SubmitJob`, `GetJob` and `ListJobs` answers (gRPC bytes
/// and JSON bodies) each show the callback exactly as its URL and deadline with an empty key,
/// and hold the key in no spelling; of every row of the store, after the job ran on a host and
/// its allocation was freed (so its events and its usage row are written), only the job's
/// `callback/` row holds the key, and after the delivery none does; the scrape's callback
/// counts are exactly the one delivery; and the log lines the calls and the delivery wrote
/// are exactly the ones below, which name the job and never the key or the URL's path or
/// query. Catches the key in an answer, an event, a usage row, a metric, a log line or any row
/// besides its own, and a key kept after its delivery ended.
#[tokio::test]
async fn the_signing_key_is_never_returned_or_logged() {
    let mut rig = clocked_rig("callback_secret", false, NEVER);
    rig.register("host-a", true);
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[status(200)]);
    let writes = accepting(rig.jobs.clone(), rig.shared.clone(), AddressRule::new([])).await;
    let recorder = Arc::new(Recorder::default());
    let deliveries = delivery(&rig.jobs, &authority, &recorder);
    let given = callback(&receiver.url("/js-done/receiver-ref?token=t0k"), KEY, None);
    let spec = v1::JobSpec {
        callback: Some(given.clone()),
        ..wire("secret-1")
    };
    let (lines, _guard) = capture();
    let mut answers: Vec<(String, Vec<u8>, Vec<v1::Job>)> = Vec::new();
    let request = SubmitJobRequest {
        spec: Some(spec.clone()),
    };
    let (http, body) = writes
        .post("/v1/jobs", &to_json("SubmitJobRequest", &request))
        .await;
    assert_eq!(http, StatusCode::CREATED);
    answers.push(("submit json".to_owned(), body, Vec::new()));
    let made: SubmitJobResponse = writes.grpc("SubmitJob", request).await.expect("same job");
    answers.push((
        "submit grpc".to_owned(),
        made.encode_to_vec(),
        made.job.into_iter().collect(),
    ));
    let id = JobId::parse("secret-1").expect("an id");
    rig.jobs
        .place(&id, HostId::new("host-a").expect("a host"), BOOKED)
        .expect("placed");
    let mut agent = FakeAgent::new("host-a");
    agent.take(&rig.beat(&agent).await.expect("a beat"));
    agent.run("secret-1");
    agent.end("secret-1", Outcome::Succeeded, Some(0));
    let ended = rig.beat(&agent).await.expect("a beat");
    agent.take(&ended);
    rig.beat(&agent).await.expect("a beat");
    assert_eq!(rig.allocation(&id).step, AllocationStep::Freed);
    for (name, uri) in [("get json", "/v1/jobs/secret-1"), ("list json", "/v1/jobs")] {
        let (http, body) = writes.http_as(TOKEN, "GET", uri, None).await;
        assert_eq!(http, StatusCode::OK, "{name}");
        answers.push((name.to_owned(), body, Vec::new()));
    }
    let request = GetJobRequest {
        job_id: "secret-1".to_owned(),
    };
    let got: GetJobResponse = writes.grpc("GetJob", request).await.expect("read");
    answers.push((
        "get grpc".to_owned(),
        got.encode_to_vec(),
        got.job.into_iter().collect(),
    ));
    let all: ListJobsResponse = writes
        .grpc("ListJobs", ListJobsRequest::default())
        .await
        .expect("read");
    answers.push(("list grpc".to_owned(), all.encode_to_vec(), all.jobs));
    for (name, bytes, jobs) in &answers {
        assert!(!holds_key(bytes), "{name}");
        for job in jobs {
            let callback = job.spec.as_ref().and_then(|spec| spec.callback.clone());
            assert_eq!(callback, Some(shown(&given)), "{name}");
        }
    }
    let usage = rig.store.scan_prefix("usage/ended/").expect("reads");
    assert_eq!(usage.len(), 1, "the job's usage row");
    assert_eq!(
        rows_holding_key(&*rig.store),
        vec!["callback/secret-1".to_owned()]
    );
    assert!(deliveries.pass(), "the callback is due");
    assert_eq!(receiver.heads().len(), 1);
    assert_eq!(rows_holding_key(&*rig.store), Vec::<String>::new());
    let text = rendered(&recorder);
    let counted = text
        .lines()
        .filter(|line| line.starts_with("js_callback_deliveries_total{"));
    let counted: Vec<&str> = counted.collect();
    assert_eq!(
        counted,
        [
            "js_callback_deliveries_total{result=\"delivered\"} 1",
            "js_callback_deliveries_total{result=\"abandoned\"} 0",
            "js_callback_deliveries_total{result=\"failed\"} 0",
            "js_callback_deliveries_total{result=\"refused-address\"} 0",
        ]
    );
    assert!(!holds_key(text.as_bytes()));
    let text = lines.take();
    assert!(!holds_key(text.as_bytes()));
    let ours = text.lines().filter(|line| {
        line.contains(" js_lease_server::callbacks: ")
            | line.contains(" js_callback::")
            | line.contains(" js_lease_server::job_api: ")
    });
    let ours: String = ours.map(|line| format!("{line}\n")).collect();
    assert_eq!(
        ours,
        " INFO js_lease_server::job_api: job service call call=\"SubmitJob\" client=\"default\" role=\"operator\" job=\"secret-1\" target=\"secret-1\" outcome=\"ok\" code=Ok\n INFO js_lease_server::job_api: job service call call=\"SubmitJob\" client=\"default\" role=\"operator\" job=\"secret-1\" target=\"secret-1\" outcome=\"ok\" code=Ok\n INFO js_lease_server::callbacks: a callback was tried job=secret-1 result=\"delivered\"\n"
    );
}

/// Asserts that a status that cannot be read (its job's `callback/` row unreadable) fails
/// `GetJob` and `ListJobs` as a failed read does (`INTERNAL`, `internal`), never a job shown
/// without it; and that `SubmitJob` of the job again, which the store answers with the job as
/// recorded, still answers it, with no status, as its commit is done. Catches a status read
/// whose failure is swallowed by a read call, and a resubmit failed by a status it only shows.
#[tokio::test]
async fn an_unreadable_status_fails_a_read_and_not_a_submit() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    let writes = accepting(Arc::new(jobs), shared, AddressRule::new([])).await;
    let spec = v1::JobSpec {
        callback: Some(callback("https://hooks.example/x", &[6; 32], None)),
        ..wire("unread-1")
    };
    writes.submit_grpc(&spec).await.expect("recorded");
    let id = JobId::parse("unread-1").expect("an id");
    let garbage = js_state::Txn::new().put(
        js_jobs::keys::callback(&id),
        vec![0xff, 0xff],
        js_state::Cond::Any,
    );
    store.txn(garbage).expect("reaches").expect("written");
    let request = GetJobRequest {
        job_id: "unread-1".to_owned(),
    };
    let failed = writes.grpc::<_, GetJobResponse>("GetJob", request).await;
    let failed = failed.expect_err("the status cannot be read");
    assert_eq!(
        (failed.code(), failed.message()),
        (Code::Internal, "internal")
    );
    let listed = writes
        .grpc::<_, ListJobsResponse>("ListJobs", ListJobsRequest::default())
        .await;
    let failed = listed.expect_err("the status cannot be read");
    assert_eq!(
        (failed.code(), failed.message()),
        (Code::Internal, "internal")
    );
    let again = writes.submit_grpc(&spec).await.expect("the same job");
    assert_eq!(again.callback_status, None);
}
