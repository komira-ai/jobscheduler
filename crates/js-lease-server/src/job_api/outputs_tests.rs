//! A job's declared outputs (`docs/design/job-outputs.md`): the submit of a job that declares
//! them, and `GetOutputs` over js-logstore's fake store, through both transports (the logs
//! rig's `read`, which holds the gateway's answer to the gRPC one). Nothing sleeps: the store's
//! clock is the service's manual clock.

use std::sync::Arc;

use js_caps::{FLAG_SET, JOB_OUTPUTS};
use js_jobs::{JobEnd, JobId, JobSpec, Output};
use js_logstore::fake::FakeStore;
use js_logstore::outputs::{Manifest, OutputFailure, OutputFile, OutputPath};
use js_logstore::{HttpRequest, Method, Transport};
use js_proto::v1::{self, GetOutputsRequest, GetOutputsResponse};
use tonic::{Code, Status};

use super::JobApi;
use super::logs;
use super::logs_rig::{ENDPOINT, Failure, Rig, prefix, sink};
use super::outputs::URL_TTL;
use super::read_tests::{
    self, START_MS, clock, over_fake, place, queue, run, service_clock, spec, start,
};
use super::submit;
use super::tests::token;
use super::writes_rig::{CEILING, Fixed, Via, Writes, rows, said, wire};
use crate::logs::StoreClock;

/// A SHA-256 as the manifest holds it.
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The code and message of a refused call.
fn refusal<T: std::fmt::Debug>(answer: Result<T, Status>) -> (Code, String) {
    let status = answer.expect_err("the call is refused");
    (status.code(), status.message().to_owned())
}

/// `GetOutputs` of `job` over both transports.
async fn outputs(rig: &Rig, job: &str) -> Result<GetOutputsResponse, Status> {
    let request = GetOutputsRequest {
        job_id: job.to_owned(),
    };
    let uri = format!("/v1/jobs/{job}/outputs");
    rig.calls.read("GetOutputs", request, &uri).await
}

/// [`spec`] declaring `outputs`.
fn declaring(outputs: &[&str]) -> JobSpec {
    JobSpec {
        outputs: outputs.iter().map(|path| (*path).to_owned()).collect(),
        ..spec(&[])
    }
}

/// Records the job `text` declaring `outputs` and takes it to running on `host-a`.
fn running_with(rig: &Rig, text: &str, outputs: &[&str]) -> JobId {
    let id = queue(rig.jobs(), text, declaring(outputs));
    place(rig.jobs(), &id);
    start(rig.jobs(), &id);
    run(rig.jobs(), &id);
    id
}

/// Ends the running job `id` with exit 0.
fn ended(rig: &Rig, id: &JobId) {
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    rig.jobs().end_job(id, end).expect("the job ends");
}

/// The manifest of `job`'s outputs: `out/report.json` put, `coverage.lcov` missing,
/// `out/core` too large.
fn manifest() -> Manifest {
    Manifest {
        version: 1,
        files: vec![OutputFile {
            path: "out/report.json".to_owned(),
            bytes: 5,
            sha256: SHA.to_owned(),
        }],
        missing: vec!["coverage.lcov".to_owned()],
        failed: vec![OutputFailure {
            path: "out/core".to_owned(),
            reason: "too-large".to_owned(),
        }],
    }
}

/// Puts `bytes` as the outputs manifest of `job`.
fn put_manifest(rig: &Rig, job: &str, bytes: &[u8]) {
    rig.put(&prefix(job).outputs_manifest_key(), bytes);
}

/// The not-ready answer for a job in `state`.
fn not_ready(state: v1::JobState) -> GetOutputsResponse {
    GetOutputsResponse {
        state: state.into(),
        ..GetOutputsResponse::default()
    }
}

/// Asserts that a job's outputs are not ready, with the job's state and no list, while it is
/// queued, while it runs, once it has ended with no manifest yet, and when it ended without
/// ever being placed (cancelled while queued); and that the store is asked only once the job
/// has ended on a host. It catches a half-written upload answered as the job's outputs, and a
/// read of the store for a job that cannot have any.
#[tokio::test]
async fn outputs_are_not_ready_until_the_manifest_is_there() {
    let rig = Rig::new().await;
    let rig = &rig;
    let answer = |job: &'static str| async move { outputs(rig, job).await.expect("an answer") };
    queue(rig.jobs(), "queued-1", declaring(&["out"]));
    assert_eq!(answer("queued-1").await, not_ready(v1::JobState::Queued));
    let id = running_with(rig, "build-1", &["out"]);
    assert_eq!(answer("build-1").await, not_ready(v1::JobState::Running));
    assert!(rig.asked().is_empty(), "{:?}", rig.asked());
    ended(rig, &id);
    assert_eq!(answer("build-1").await, not_ready(v1::JobState::Succeeded));
    let manifest_key = prefix("build-1").outputs_manifest_key();
    assert_eq!(rig.asked(), [manifest_key.clone(), manifest_key]);
    let cancelled = queue(rig.jobs(), "never-1", declaring(&["out"]));
    rig.jobs()
        .request_cancel(&cancelled)
        .expect("a queued job is cancelled");
    assert_eq!(answer("never-1").await, not_ready(v1::JobState::Cancelled));
}

/// Asserts the answer once the manifest is there: ready, the job's state, each file with its
/// length, digest and a presigned URL that the store answers with the file's bytes and nothing
/// else (another key with the same signature is refused), the missing paths and the failures,
/// and the URLs' expiry an hour after the service's clock. It catches a URL that reads more
/// than the one object, and a file left out of the answer.
#[tokio::test]
async fn a_written_manifest_is_answered_with_a_url_for_each_file() {
    let rig = Rig::new().await;
    let id = running_with(&rig, "build-2", &["out", "coverage.lcov"]);
    ended(&rig, &id);
    let file = prefix("build-2").output_key(&OutputPath::parse("out/report.json").unwrap());
    rig.put(&file, b"hello");
    rig.put(
        &prefix("build-2").output_key(&OutputPath::parse("out/other").unwrap()),
        b"no",
    );
    put_manifest(&rig, "build-2", &manifest().encode());
    let answer = outputs(&rig, "build-2").await.expect("the outputs");
    let ttl = u64::try_from(URL_TTL.as_millis()).unwrap();
    let now = js_state::Clock::now(&*rig.calls.clock).as_duration();
    let now_ms = u64::try_from(now.as_millis()).unwrap();
    assert!(now_ms > START_MS);
    assert_eq!(
        (answer.state, answer.ready, answer.urls_expire_at_ms),
        (v1::JobState::Succeeded.into(), true, Some(now_ms + ttl))
    );
    assert_eq!(answer.missing, ["coverage.lcov"]);
    assert_eq!(
        answer.failed,
        [v1::OutputFailure {
            path: "out/core".to_owned(),
            reason: "too-large".to_owned(),
        }]
    );
    let [got] = answer.files.as_slice() else {
        panic!("one file: {:?}", answer.files);
    };
    assert_eq!(
        (got.path.as_str(), got.bytes, got.sha256.as_str()),
        ("out/report.json", 5, SHA)
    );
    assert!(
        got.url.starts_with(&format!("{ENDPOINT}/logs/{file}?")),
        "{}",
        got.url
    );
    assert!(got.url.contains("X-Amz-Expires=3600"), "{}", got.url);
    let fetch = |url: &str| {
        let request = HttpRequest {
            method: Method::Get,
            url: url.to_owned(),
            headers: vec![("host".to_owned(), "objects.example".to_owned())],
            body: Vec::new(),
        };
        rig.store.send(request).expect("the store answers")
    };
    let read = fetch(&got.url);
    assert_eq!((read.status, read.body.as_slice()), (200, &b"hello"[..]));
    let other = fetch(&got.url.replace("report.json", "other"));
    assert_eq!(other.status, 403);
}

/// Asserts the answer of a manifest with no file: ready, the missing paths, and no expiry. It
/// catches an expiry given for URLs that do not exist.
#[tokio::test]
async fn a_manifest_with_no_file_has_no_expiry() {
    let rig = Rig::new().await;
    let id = running_with(&rig, "build-3", &["out"]);
    ended(&rig, &id);
    let empty = Manifest {
        version: 1,
        missing: vec!["out".to_owned()],
        ..Manifest::default()
    };
    put_manifest(&rig, "build-3", &empty.encode());
    let answer = outputs(&rig, "build-3").await.expect("the outputs");
    assert_eq!(
        answer,
        GetOutputsResponse {
            state: v1::JobState::Succeeded.into(),
            ready: true,
            missing: vec!["out".to_owned()],
            ..GetOutputsResponse::default()
        }
    );
}

/// Asserts each manifest the server cannot read, all answered `UNAVAILABLE`
/// `outputs-unreadable` (503 over the gateway) with nothing of the manifest in the answer: one
/// that is not JSON, one of another version, one listing a file outside the job's outputs,
/// and one longer than 1 MiB. It catches a manifest that would hand a client a URL to another
/// key of the store.
#[tokio::test]
async fn a_manifest_that_is_not_the_jobs_is_unreadable() {
    let rig = Rig::new().await;
    let id = running_with(&rig, "build-4", &["out"]);
    ended(&rig, &id);
    let mut stray = manifest();
    stray.files[0].path = "elsewhere/secret".to_owned();
    let long = vec![b' '; 1024 * 1024 + 1];
    let unreadable = (Code::Unavailable, "outputs-unreadable".to_owned());
    for bytes in [
        b"not json".to_vec(),
        br#"{"version":2}"#.to_vec(),
        stray.encode(),
        long,
    ] {
        put_manifest(&rig, "build-4", &bytes);
        assert_eq!(refusal(outputs(&rig, "build-4").await), unreadable);
    }
}

/// Asserts the refusals before the store is read: a job that declared no outputs is
/// `no-outputs`, a job whose id gives no object prefix (so it never had a sink) is
/// `no-outputs`, an unknown job and one past its retention are answered as the other reads
/// answer them, and a server without an object store is `no-log-sink`. It catches a read of
/// the store for a job that cannot have outputs.
#[tokio::test]
async fn jobs_that_cannot_have_outputs_are_refused_by_name() {
    let rig = Rig::new().await;
    let plain = read_tests::running(rig.jobs(), "plain-1");
    ended(&rig, &plain);
    let no_outputs = (Code::FailedPrecondition, "no-outputs".to_owned());
    assert_eq!(refusal(outputs(&rig, "plain-1").await), no_outputs);
    let odd = running_with(&rig, "-odd", &["out"]);
    ended(&rig, &odd);
    assert_eq!(refusal(outputs(&rig, "-odd").await), no_outputs);
    assert!(rig.asked().is_empty(), "{:?}", rig.asked());
    let unknown = (Code::NotFound, "unknown-job".to_owned());
    assert_eq!(refusal(outputs(&rig, "nobody-1").await), unknown);
    rig.advance(rig.jobs().retention().window());
    let expired = (Code::NotFound, "expired".to_owned());
    assert_eq!(refusal(outputs(&rig, "plain-1").await), expired);

    let clock = clock();
    let jobs = over_fake(&clock);
    let bare = read_tests::Rig::over(jobs, clock).await;
    queue(&bare.jobs, "kept-1", declaring(&["out"]));
    let request = GetOutputsRequest {
        job_id: "kept-1".to_owned(),
    };
    let answer = bare
        .read::<_, GetOutputsResponse>("GetOutputs", request, "/v1/jobs/kept-1/outputs")
        .await;
    let no_sink = (Code::FailedPrecondition, "no-log-sink".to_owned());
    assert_eq!(refusal(answer), no_sink);
}

/// Asserts that a store that cannot be read, by a refused connection, a 503 or a 403, answers
/// `UNAVAILABLE` `log-store-unavailable` and nothing of the store's answer. It catches a
/// store's error text passed to a client.
#[tokio::test]
async fn a_store_that_cannot_be_read_is_unavailable() {
    let rig = Rig::new().await;
    let id = running_with(&rig, "build-5", &["out"]);
    ended(&rig, &id);
    put_manifest(&rig, "build-5", &manifest().encode());
    let down = (Code::Unavailable, "log-store-unavailable".to_owned());
    let request = || GetOutputsRequest {
        job_id: "build-5".to_owned(),
    };
    for failure in [
        Failure::Refused,
        Failure::Answer(503, "SlowDown"),
        Failure::Answer(403, "AccessDenied"),
    ] {
        rig.tap.fail_after(0, failure);
        let answer = rig
            .calls
            .grpc::<_, GetOutputsResponse>("GetOutputs", request())
            .await;
        assert_eq!(refusal(answer), down, "{failure:?}");
        rig.tap.fail_after(0, failure);
        let (status, body) = rig.calls.get("/v1/jobs/build-5/outputs").await;
        assert_eq!(
            (status.as_u16(), read_tests::json_of(&body)),
            (503, serde_json::json!({"error": "log-store-unavailable"})),
            "{failure:?}"
        );
    }
}

/// The job service over fresh tables, its submit served admitting every job (`admission`),
/// with an object-store sink when `with_sink`.
async fn submitting(admission: Arc<Fixed>, with_sink: bool) -> Writes {
    let clock = clock();
    let jobs = over_fake(&clock);
    let shared = service_clock(&clock);
    let mut api =
        JobApi::new(jobs, shared.clone()).with_submit(submit::Ctx::served(CEILING, admission));
    if with_sink {
        let store = FakeStore::new(
            js_logstore::Endpoint::parse(ENDPOINT, "us-east-1", "logs", true).unwrap(),
            Arc::new(StoreClock(shared.clone())),
        );
        let read = store.minting_key();
        let transport: Arc<dyn Transport> = Arc::new(store);
        api = api.with_logs(logs::Ctx::over(transport, &sink(1), read, &shared));
    }
    Writes::over(api, token()).await
}

/// [`wire`] declaring `outputs`.
fn wire_with(id: &str, outputs: &[&str]) -> v1::JobSpec {
    v1::JobSpec {
        outputs: outputs.iter().map(|path| (*path).to_owned()).collect(),
        ..wire(id)
    }
}

/// Asserts that a job with outputs is refused `FAILED_PRECONDITION` `no-log-sink` (409 over
/// the gateway) by a server without an object store, before admission is asked and with
/// nothing recorded, while a job without outputs is taken there. It catches a job whose
/// outputs could never be read back taken on.
#[tokio::test]
async fn a_job_with_outputs_needs_an_object_store() {
    let admission = Fixed::new(true);
    let rig = submitting(Arc::clone(&admission), false).await;
    for via in Via::BOTH {
        let answer = rig.submit(via, &wire_with("", &["out"])).await;
        let wants = said(&Status::failed_precondition("no-log-sink"));
        assert_eq!(answer.expect_err("refused"), wants, "{via:?}");
    }
    assert_eq!((rows(&rig.jobs), admission.asked()), (0, 0));
    rig.submit(Via::Grpc, &wire("plain-1"))
        .await
        .expect("a job without outputs is taken");
}

/// Asserts that a server with an object store takes a job with outputs: the job's record keeps
/// the paths in order and requires the capability `job-outputs`, and a read of the job shows the
/// paths; and that a declaration that breaks the store's rules is refused `INVALID_ARGUMENT`
/// naming the first path at fault, `field: spec.outputs[1].path`, with nothing recorded. It
/// catches outputs dropped at submit, a job placed on a host that cannot upload them, and a
/// path that could leave the job's directory taken.
#[tokio::test]
async fn a_server_with_an_object_store_takes_and_checks_a_jobs_outputs() {
    let rig = submitting(Fixed::new(true), true).await;
    let job = rig
        .submit_grpc(&wire_with("build-1", &["out", "report.json"]))
        .await
        .expect("taken");
    let shown = job.spec.expect("the job's spec");
    assert_eq!(shown.outputs, ["out", "report.json"]);
    let record = rig
        .jobs
        .get_job(&read_tests::id("build-1"))
        .expect("read")
        .expect("recorded");
    assert_eq!(record.spec.outputs, ["out", "report.json"]);
    assert_eq!(
        record
            .spec
            .constraints
            .capabilities
            .get(JOB_OUTPUTS)
            .map(String::as_str),
        Some(FLAG_SET)
    );
    let before = rows(&rig.jobs);
    for bad in [&["out", "../etc"][..], &["out", "out/inner"], &["a", "a b"]] {
        let answer = rig.submit_grpc(&wire_with("", bad)).await;
        let field = (
            Code::InvalidArgument,
            "field: spec.outputs[1].path".to_owned(),
        );
        assert_eq!(refusal(answer), field, "{bad:?}");
    }
    assert_eq!(rows(&rig.jobs), before);
    let plain = rig.submit_grpc(&wire("plain-2")).await.expect("taken");
    let plain = rig
        .jobs
        .get_job(&read_tests::id(&plain.id))
        .expect("read")
        .expect("recorded");
    assert_eq!(plain.spec.constraints.capabilities.get(JOB_OUTPUTS), None);
}
