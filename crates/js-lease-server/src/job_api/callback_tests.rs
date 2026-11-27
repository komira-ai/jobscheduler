//! The job service with its submit taking completion callbacks, judged by an address rule the
//! test gives (`docs/design/job-retry-and-completion-callback.md`, sections 5.2, 5.5 and 7): the
//! submit's checks of a callback, the status `GetJob` and `ListJobs` show, and
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
use super::read_tests::{from_json, json_of};
use super::submit::{Admission, Ctx};
use super::tests::{TOKEN, token};
use super::writes_rig::{CEILING, Fixed, NEVER, Via, Writes, rows, to_json, wire};
use crate::agent_api::job_rig::{
    BOOKED, FakeAgent, OneClock, Rig, START_MS, clocked_rig, tables_on,
};
use crate::api::rig::{Lines, process_sink};
use crate::callbacks::Delivery;
use crate::callbacks::receiver::{Authority, Loopback, Receiver, status};
use crate::clock::SharedClock;
use crate::job_store::ServerJobs;
use crate::metrics::{Meter, NoMetrics, Recorder, rendered};

/// The key the redaction test's job is signed with: 40 bytes no answer, row or line may hold.
const KEY: &[u8; 40] = b"never-shown-callback-signing-key-0123456";

/// The job service over `jobs` and `clock`, admitting every job, its submit judging a
/// callback's address by `rule`.
async fn accepting(jobs: Arc<ServerJobs>, clock: SharedClock, rule: AddressRule) -> Writes {
    let admission: Arc<dyn Admission> = Fixed::new(true);
    let submit = Ctx::served(CEILING, admission).judging_callbacks_by(rule);
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

/// The job `id` as the gateway's `GET /v1/jobs/{id}` answers it.
async fn read_json(writes: &Writes, id: &str) -> v1::Job {
    let (http, body) = writes
        .http_as(TOKEN, "GET", &format!("/v1/jobs/{id}"), None)
        .await;
    assert_eq!(http, StatusCode::OK, "{id}");
    let got: GetJobResponse = from_json("jobscheduler.v1.GetJobResponse", &body);
    got.job.expect("a job")
}

/// The state of `job`'s callback, and its tries.
fn callback_state(job: &v1::Job) -> (v1::CallbackState, u32) {
    let status = job.callback_status.as_ref().expect("a callback status");
    (status.state(), status.tries)
}

/// One heartbeat of each of `agents`, their replies taken.
async fn beat_all(rig: &Rig, agents: &mut [FakeAgent]) {
    for agent in agents.iter_mut() {
        let reply = rig.beat(agent).await.expect("a beat");
        agent.take(&reply);
    }
}

/// Asserts the whole path of a job with a retry policy and a completion callback, end to end
/// through the JSON gateway, the job pass, the placer, two hosts' agents and a TLS receiver on
/// loopback, on a clock only the test moves. The submit is taken (`201`), the answer shows the
/// policy as sent and the callback with no key, and the callback is armed. The job runs on
/// the host it is placed on; that host says hello again and lists nothing (its agent lost
/// the job), and `GET /v1/jobs/{id}` then shows the job queued for its second attempt: attempt
/// 1, one entry in its history (`HOST_LOST`, on that host, retried), a time before which it
/// waits, and the callback still armed, and a delivery pass sends nothing. Once the backoff
/// has passed the job is placed on the other host as attempt 2, runs and succeeds, and the
/// read shows it succeeded, attempt 2, both attempts in its history (the second succeeded, not
/// retried), and its callback pending with no try. One delivery pass then sends exactly one
/// signed POST with an empty body to the path the job named, a second pass sends nothing, the
/// read shows the callback delivered after one try, and the scrape counts one delivery. Catches
/// a gateway that still refuses either field, a lost host that ends a job with attempts left,
/// a retry placed on the host it failed on, a callback sent per attempt or before the final
/// end, one never sent after it, and one sent twice.
#[tokio::test]
async fn a_job_whose_host_is_lost_is_retried_and_called_back_once() {
    let grace = Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS);
    let step = Duration::from_secs(10);
    let mut rig = clocked_rig("callback_after_retry", true, NEVER);
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut agents = [FakeAgent::new("host-a"), FakeAgent::new("host-b")];
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[status(200)]);
    let writes = accepting(rig.jobs.clone(), rig.shared.clone(), AddressRule::new([])).await;
    let recorder = Arc::new(Recorder::default());
    let deliveries = delivery(&rig.jobs, &authority, &recorder);
    let given = callback(&receiver.url("/js-done/ref-1"), &[5; 32], None);
    let policy = v1::RetryPolicy {
        max_attempts: 2,
        initial_backoff_seconds: Some(30),
        max_backoff_seconds: Some(30),
    };
    let request = SubmitJobRequest {
        spec: Some(v1::JobSpec {
            retry: Some(policy),
            callback: Some(given.clone()),
            ..wire("lost-1")
        }),
    };
    let (http, body) = writes
        .post("/v1/jobs", &to_json("SubmitJobRequest", &request))
        .await;
    assert_eq!(http, StatusCode::CREATED);
    let made: SubmitJobResponse = from_json("jobscheduler.v1.SubmitJobResponse", &body);
    let made = made.job.expect("a job");
    let echoed = made.spec.clone().expect("a spec");
    assert_eq!(
        (echoed.retry, echoed.callback),
        (Some(policy), Some(shown(&given)))
    );
    assert_eq!(made.state(), v1::JobState::Queued);
    assert_eq!(callback_state(&made), (v1::CallbackState::Armed, 0));

    // Attempt 1 is placed, taken and run.
    let id = JobId::parse("lost-1").expect("an id");
    rig.tick();
    let first = rig.allocation(&id).host.as_str().to_owned();
    let (lost, other) = match first.as_str() {
        "host-a" => (0, 1),
        _ => (1, 0),
    };
    beat_all(&rig, &mut agents).await;
    agents[lost].run("lost-1");
    beat_all(&rig, &mut agents).await;
    assert_eq!(
        read_json(&writes, "lost-1").await.state(),
        v1::JobState::Running
    );

    // Its host says hello again and lists nothing: the attempt's host is lost, and the job is
    // queued for attempt 2 with its callback still armed.
    rig.advance(grace + step);
    rig.register(&first, true);
    agents[lost] = FakeAgent::new(&first);
    beat_all(&rig, &mut agents).await;
    let waiting = read_json(&writes, "lost-1").await;
    assert_eq!(waiting.state(), v1::JobState::Queued);
    assert_eq!(waiting.attempt, 1);
    let ended: Vec<_> = waiting
        .attempt_history
        .iter()
        .map(|a| (a.number, a.host_id.as_str(), a.outcome(), a.retried))
        .collect();
    assert_eq!(
        ended,
        [(1, first.as_str(), v1::AttemptOutcome::HostLost, true)]
    );
    assert!(waiting.retry_not_before_ms.is_some());
    assert_eq!(callback_state(&waiting), (v1::CallbackState::Armed, 0));
    assert!(!deliveries.pass(), "nothing is due at a retry");
    assert_eq!(receiver.heads(), Vec::<String>::new());

    // After the backoff, attempt 2 is placed on the other host, runs and succeeds.
    for _ in 0..4 {
        rig.advance(step);
        rig.tick();
        beat_all(&rig, &mut agents).await;
    }
    let again = rig.allocation(&id);
    assert_eq!(again.host.as_str(), agents[other].host);
    assert_eq!(read_json(&writes, "lost-1").await.attempt, 2);
    agents[other].run("lost-1");
    beat_all(&rig, &mut agents).await;
    agents[other].end("lost-1", Outcome::Succeeded, Some(0));
    beat_all(&rig, &mut agents).await;
    beat_all(&rig, &mut agents).await;
    assert_eq!(rig.allocation(&id).step, AllocationStep::Freed);
    let done = read_json(&writes, "lost-1").await;
    assert_eq!(done.state(), v1::JobState::Succeeded);
    assert_eq!(done.attempt, 2);
    let outcomes: Vec<_> = done
        .attempt_history
        .iter()
        .map(|a| (a.number, a.outcome(), a.retried))
        .collect();
    assert_eq!(
        outcomes,
        [
            (1, v1::AttemptOutcome::HostLost, true),
            (2, v1::AttemptOutcome::Succeeded, false),
        ]
    );
    assert_eq!(callback_state(&done), (v1::CallbackState::Pending, 0));

    // One delivery, of one bare signed POST, and none after it.
    assert!(deliveries.pass(), "the callback is due at the final end");
    let heads = receiver.heads();
    assert_eq!(heads.len(), 1, "{heads:?}");
    assert!(
        heads[0].starts_with("POST /js-done/ref-1 HTTP/1.1\r\n"),
        "{}",
        heads[0]
    );
    for header in [
        "webhook-id: ",
        "webhook-timestamp: ",
        "webhook-signature: v1,",
    ] {
        assert!(heads[0].contains(&format!("\r\n{header}")), "{header}");
    }
    assert!(!deliveries.pass(), "a delivered callback is not sent again");
    assert_eq!(receiver.heads(), Vec::<String>::new());
    assert_eq!(
        callback_state(&read_json(&writes, "lost-1").await),
        (v1::CallbackState::Delivered, 1)
    );
    let text = rendered(&recorder);
    let delivered = "js_callback_deliveries_total{result=\"delivered\"} 1";
    assert!(text.lines().any(|line| line == delivered), "{text}");
}
