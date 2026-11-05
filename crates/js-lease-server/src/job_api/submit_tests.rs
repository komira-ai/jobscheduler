//! `SubmitJob` (`submit.rs`) over both transports. Jobs are moved through their states by the
//! agent rig's server (its placement pass and its fake agent) or by the job tables directly.
//! The rig's registry, server and store read one manual clock; nothing sleeps.

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::http::StatusCode;
use js_agent_wire::Outcome;
use js_jobs::JobState;
use js_proto::refusal::{refusal_of, refusal_status};
use js_proto::v1::{self, RefusalCode, SubmitJobRequest};
use serde_json::{Value, json};
use tonic::{Code, Request, Status};

use super::submit_job;
use crate::admit::{Admit, JobAdmission};
use crate::agent_api::job_rig::{FakeAgent, clocked_rig};
use crate::job_api::JobApi;
use crate::job_api::submit::Ctx;
use crate::job_api::writes_rig::{
    CEILING, Fixed, NEVER, Via, Writes, id, over_fake, rows, said, served, to_json, wire,
};

/// The answer of a refusal by `code`, as the gateway says it, beside its gRPC code.
fn refusal(code: RefusalCode) -> (StatusCode, Value) {
    said(&refusal_status(code, ""))
}

/// Asserts that `status` is the refusal `code`, with its gRPC code.
fn is_refusal(status: &Status, code: RefusalCode, grpc: Code) {
    assert_eq!(status.code(), grpc, "{status:?}");
    let carried = refusal_of(status).expect("the status carries a refusal");
    assert_eq!(carried.code(), code);
}

/// Asserts that a submit repeated after its answer was lost, with its id or with the id the
/// first answer minted, over gRPC and over JSON, is answered with the very job of the first
/// (id, state, times), that the tables hold one row, and that the placement pass that
/// follows makes exactly one placement. Row 1 of the design's failure list. It catches a
/// retry that inserts a second row and a second answer that differs in id or state.
#[tokio::test]
async fn a_retry_after_a_lost_answer_makes_one_job() {
    for via in Via::BOTH {
        for named in ["build-1", ""] {
            let name = format!("submit_retry_{via:?}_{}", named.is_empty());
            let mut rig = clocked_rig(&name, true, NEVER);
            rig.register("host-a", true);
            let writes = served(&rig).await;
            let lost = writes.submit(via, &wire(named)).await.unwrap();
            assert_eq!(lost.state(), v1::JobState::Queued);
            if !named.is_empty() {
                assert_eq!(lost.id, named);
            }
            let again = writes.submit(via, &wire(&lost.id)).await.unwrap();
            assert_eq!(again, lost, "{via:?} {named:?}");
            assert_eq!(rows(&rig.jobs), 1);
            rig.tick();
            rig.tick();
            assert_eq!(rig.jobs.live_allocations().unwrap().len(), 1);
            assert_eq!(rig.job(&id(&lost.id)).state, JobState::Placed);
        }
    }
}

/// Asserts that a retry is answered from the job as it is now: running once its agent runs
/// it, succeeded once it ended, never reset to queued, over both transports, with one row. It
/// catches a retry answered `queued` from the request and one that resets the job.
#[tokio::test]
async fn a_retry_is_answered_in_whatever_state_the_job_is_now() {
    let mut rig = clocked_rig("submit_retry_state", true, NEVER);
    rig.register("host-a", true);
    let writes = served(&rig).await;
    let spec = wire("job-r");
    writes.submit(Via::Grpc, &spec).await.unwrap();
    rig.tick();
    let mut agent = FakeAgent::new("host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-r");
    agent.take(&rig.beat(&agent).await.unwrap());
    for via in Via::BOTH {
        let job = writes.submit(via, &spec).await.unwrap();
        assert_eq!(
            (job.state(), job.host_id.as_str()),
            (v1::JobState::Running, "host-a")
        );
    }
    assert_eq!(rig.job(&id("job-r")).state, JobState::Running);
    agent.end("job-r", Outcome::Succeeded, Some(0));
    agent.take(&rig.beat(&agent).await.unwrap());
    for via in Via::BOTH {
        let job = writes.submit(via, &spec).await.unwrap();
        assert_eq!(job.state(), v1::JobState::Succeeded);
    }
    assert_eq!(rig.job(&id("job-r")).state, JobState::Succeeded);
    assert_eq!(rows(&rig.jobs), 1);
}

/// The job `x` every conflict case changes one field of: a network level, a port, an
/// environment variable, a label and a file beside [`wire`]'s fields.
fn conflicted() -> v1::JobSpec {
    v1::JobSpec {
        env: HashMap::from([("MODE".to_owned(), "fast".to_owned())]),
        network: v1::NetworkLevel::AllowList.into(),
        ports: vec![v1::PortForward {
            host_port: 8080,
            container_port: 80,
        }],
        files: vec![v1::JobFile {
            name: "token".to_owned(),
            contents: "a".to_owned(),
        }],
        labels: HashMap::from([("team".to_owned(), "build".to_owned())]),
        ..wire("x")
    }
}

/// Asserts that the same id with a job that differs in any one field (image, command,
/// arguments, environment, CPU, memory, wall limit, label, network, port, file) is refused
/// `job-id-conflict` over both transports (`ALREADY_EXISTS` with the refusal, 409), that the
/// stored job is unchanged, and that admission is not asked for a held id. It catches a field
/// left out of the comparison.
#[tokio::test]
async fn same_id_different_job_is_a_conflict() {
    let fixed = Fixed::new(true);
    let (_, writes) = over_fake(Arc::clone(&fixed)).await;
    writes.submit(Via::Grpc, &conflicted()).await.unwrap();
    let before = writes.jobs.get_job(&id("x")).unwrap();
    let needs = |cpu: u64, memory: u64| {
        Some(v1::Needs {
            cpu_millis: cpu,
            memory_bytes: memory,
            ..v1::Needs::default()
        })
    };
    let changes: Vec<(&str, Box<dyn Fn(&mut v1::JobSpec)>)> = vec![
        (
            "image",
            Box::new(|spec| spec.image = format!("team/app@sha256:{}", "b".repeat(64))),
        ),
        ("command", Box::new(|spec| spec.command = "walk".to_owned())),
        ("args", Box::new(|spec| spec.args.push("--slow".to_owned()))),
        (
            "env",
            Box::new(|spec| {
                spec.env.insert("MODE".to_owned(), "slow".to_owned());
            }),
        ),
        (
            "cpu",
            Box::new(move |spec| spec.needs = needs(1501, 1 << 30)),
        ),
        (
            "memory",
            Box::new(move |spec| spec.needs = needs(1500, (1 << 30) + 1)),
        ),
        ("wall", Box::new(|spec| spec.wall_limit_seconds = Some(601))),
        (
            "label",
            Box::new(|spec| {
                spec.labels.insert("team".to_owned(), "test".to_owned());
            }),
        ),
        (
            "network",
            Box::new(|spec| spec.network = v1::NetworkLevel::Open.into()),
        ),
        ("port", Box::new(|spec| spec.ports[0].host_port = 8081)),
        (
            "file",
            Box::new(|spec| spec.files[0].contents = "b".to_owned()),
        ),
    ];
    for (field, change) in changes {
        let mut spec = conflicted();
        change(&mut spec);
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        is_refusal(&status, RefusalCode::JobIdConflict, Code::AlreadyExists);
        let answer = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            answer,
            (StatusCode::CONFLICT, json!({ "error": "job-id-conflict" })),
            "{field}"
        );
    }
    assert_eq!(writes.jobs.get_job(&id("x")).unwrap(), before);
    assert_eq!(fixed.asked(), 1, "only the first submit is admitted");
}

/// Asserts that the same id and job with another priority is answered from the job already
/// made, over both transports, with the stored priority, which is not overwritten. It catches
/// a priority compared or written by a retry.
#[tokio::test]
async fn priority_alone_does_not_make_a_conflict() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let first = v1::JobSpec {
        priority: 1,
        ..wire("x")
    };
    let made = writes.submit(Via::Grpc, &first).await.unwrap();
    let later = v1::JobSpec {
        priority: 9,
        ..wire("x")
    };
    for via in Via::BOTH {
        let answer = writes.submit(via, &later).await.unwrap();
        assert_eq!(answer, made, "{via:?}");
        assert_eq!(answer.spec.unwrap().priority, 1);
    }
    assert_eq!(writes.jobs.get_job(&id("x")).unwrap().unwrap().priority, 1);
    assert_eq!(rows(&writes.jobs), 1);
}

/// Asserts that an id is held until the retention sweep deletes its job: a different job
/// under the id of a job that ended at T is `job-id-conflict` at T + 7 d - 1 s, and once the
/// sweep has run at T + 7 d it is a new job, queued, with the new job's command. It catches
/// an id reusable before the retention end, and one never reusable.
#[tokio::test]
async fn an_id_is_held_until_the_retention_end_and_reusable_after() {
    let rig = clocked_rig("submit_id_held", false, NEVER);
    let writes = Writes::new(Arc::clone(&rig.jobs), rig.shared.clone(), Fixed::new(true)).await;
    writes.submit(Via::Grpc, &wire("x")).await.unwrap();
    let ended = writes.cancel(Via::Grpc, "x").await.unwrap();
    assert_eq!(ended.state(), v1::JobState::Cancelled);
    let other = v1::JobSpec {
        command: "walk".to_owned(),
        ..wire("x")
    };
    let window = rig.jobs.retention().window();
    rig.advance(window - Duration::from_secs(1));
    rig.tick();
    for via in Via::BOTH {
        let answer = writes.submit(via, &other).await.unwrap_err();
        assert_eq!(answer, refusal(RefusalCode::JobIdConflict), "{via:?}");
    }
    rig.advance(Duration::from_secs(1));
    rig.tick();
    let made = writes.submit(Via::Json, &other).await.unwrap();
    assert_eq!(made.state(), v1::JobState::Queued);
    assert_eq!(made.spec.unwrap().command, "walk");
    assert_eq!(rows(&rig.jobs), 1);
}

/// Asserts that a commit that fails answers `INTERNAL` with the message `state-write-failed`
/// over gRPC and 500 `state-write-failed` over JSON, with no row written, and that a retry
/// once the fault has cleared makes exactly one job, with an id and with a minted one; and
/// that a failed read of the id answers the same and writes nothing. It catches a row written
/// before the commit, and an answer of `internal`, which the gateway could not tell from a
/// bug.
#[tokio::test]
async fn a_failed_commit_answers_500_and_changes_nothing() {
    let fixed = Fixed::new(true);
    let (store, writes) = over_fake(Arc::clone(&fixed)).await;
    store.fail_next();
    let status = writes.submit_grpc(&wire("job-f")).await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::Internal, "state-write-failed")
    );
    assert_eq!(rows(&writes.jobs), 0);
    store.fail_next();
    let answer = writes.submit(Via::Json, &wire("job-f")).await.unwrap_err();
    let failed = (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": "state-write-failed" }),
    );
    assert_eq!(answer, failed);
    assert_eq!(rows(&writes.jobs), 0);
    let made = writes.submit(Via::Json, &wire("job-f")).await.unwrap();
    assert_eq!((made.id.as_str(), rows(&writes.jobs)), ("job-f", 1));

    store.fail_next();
    let answer = writes.submit(Via::Grpc, &wire("")).await.unwrap_err();
    assert_eq!(answer, failed);
    assert_eq!(rows(&writes.jobs), 1);
    writes.submit(Via::Grpc, &wire("")).await.unwrap();
    assert_eq!(rows(&writes.jobs), 2);

    store.fail_read_after(0);
    let answer = writes.submit(Via::Grpc, &wire("job-g")).await.unwrap_err();
    assert_eq!(answer, failed);
    assert_eq!(rows(&writes.jobs), 2);
}

/// Asserts that `SubmitJob` answers on its first poll, with the job queued and no allocation,
/// while the placement pass never runs: the handler awaits nothing but the store and the
/// hosts' admission, so it cannot be waiting for placement, a launch or a provider. It
/// catches a handler that awaits placement or anything after the commit.
#[test]
fn submit_does_not_wait_for_placement() {
    let mut rig = clocked_rig("submit_no_wait", true, NEVER);
    rig.register("host-a", true);
    let admission = JobAdmission::new(Admit::default(), rig.agents.registry.clone());
    let ctx = Ctx::served(CEILING, Arc::new(admission));
    let api = JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_submit(ctx);
    let request = Request::new(SubmitJobRequest {
        spec: Some(wire("job-w")),
    });
    let mut call = pin!(submit_job(&api, request));
    let polled = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    let state = polled.map(|answer| answer.unwrap().into_inner().job.unwrap().state());
    assert_eq!(state, Poll::Ready(v1::JobState::Queued));
    assert_eq!(rig.jobs.live_allocations().unwrap(), Vec::new());
    assert_eq!(rig.job(&id("job-w")).state, JobState::Queued);
}

/// Asserts the wall limit's refusals over both transports: absent and 0 are
/// `wall-time-not-positive`, the ceiling plus a second `wall-time-above-ceiling`, each 400
/// with the field `spec.wallLimitSeconds` (`INVALID_ARGUMENT` with the refusal over gRPC); a
/// negative limit, which only JSON can spell, is refused 400 by the gateway; exactly the
/// ceiling is accepted; and no refusal records anything or asks admission. It catches an
/// off-by-one at the ceiling and an absent limit read as unlimited.
#[tokio::test]
async fn wall_limit_missing_zero_negative_or_above_the_ceiling_is_refused() {
    let fixed = Fixed::new(true);
    let (_, writes) = over_fake(Arc::clone(&fixed)).await;
    let ceiling = CEILING.as_secs();
    let cases = [
        (None, RefusalCode::WallTimeNotPositive),
        (Some(0), RefusalCode::WallTimeNotPositive),
        (Some(ceiling + 1), RefusalCode::WallTimeAboveCeiling),
    ];
    for (wall, code) in cases {
        let spec = v1::JobSpec {
            wall_limit_seconds: wall,
            ..wire("job-w")
        };
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        is_refusal(&status, code, Code::InvalidArgument);
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(http, StatusCode::BAD_REQUEST, "{wall:?}");
        assert_eq!(body["error"], code.as_str(), "{wall:?}");
        assert_eq!(body["field"], "spec.wallLimitSeconds", "{wall:?}");
    }
    let mut negative = to_json(
        "SubmitJobRequest",
        &SubmitJobRequest {
            spec: Some(wire("job-w")),
        },
    );
    negative["spec"]["wallLimitSeconds"] = json!("-1");
    let (http, _) = writes.post("/v1/jobs", &negative).await;
    assert_eq!(http, StatusCode::BAD_REQUEST);
    assert_eq!((rows(&writes.jobs), fixed.asked()), (0, 0));
    for (via, named) in [(Via::Grpc, "at-g"), (Via::Json, "at-j")] {
        let spec = v1::JobSpec {
            wall_limit_seconds: Some(ceiling),
            ..wire(named)
        };
        let made = writes.submit(via, &spec).await.unwrap();
        assert_eq!(made.spec.unwrap().wall_limit_seconds, Some(ceiling));
    }
    assert_eq!((rows(&writes.jobs), fixed.asked()), (2, 2));
}

/// Asserts admission over the registry's hosts and no launching provider: with no host, a
/// job is refused `no-capable-host` (`FAILED_PRECONDITION` with the refusal, 422) and nothing
/// is recorded; with a host too small for it, the same; and once a capable host has
/// registered, the same submit makes the job, under the same id. It catches admission skipped,
/// a row written before it, and an id consumed by a refusal.
#[tokio::test]
async fn a_job_nothing_could_serve_is_refused_and_nothing_is_recorded() {
    let mut rig = clocked_rig("submit_admission", false, NEVER);
    let writes = served(&rig).await;
    let status = writes.submit_grpc(&wire("job-n")).await.unwrap_err();
    is_refusal(
        &status,
        RefusalCode::NoCapableHost,
        Code::FailedPrecondition,
    );
    let answer = writes.submit(Via::Json, &wire("job-n")).await.unwrap_err();
    let refused = (
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "error": "no-capable-host" }),
    );
    assert_eq!(answer, refused);
    assert_eq!(rows(&rig.jobs), 0);
    rig.register("host-a", true);
    let big = v1::JobSpec {
        needs: Some(v1::Needs {
            cpu_millis: 1000,
            memory_bytes: 32 << 30,
            ..v1::Needs::default()
        }),
        ..wire("job-big")
    };
    for via in Via::BOTH {
        assert_eq!(writes.submit(via, &big).await.unwrap_err(), refused);
    }
    assert_eq!(rows(&rig.jobs), 0);
    let made = writes.submit(Via::Json, &wire("job-n")).await.unwrap();
    assert_eq!(
        (made.id.as_str(), made.state()),
        ("job-n", v1::JobState::Queued)
    );
    assert_eq!(rows(&rig.jobs), 1);
}

/// Asserts the job id's grammar at submit: an empty id is minted (32 hex digits); 64 bytes,
/// a space, a slash and a non-ASCII letter are refused `INVALID_ARGUMENT` naming
/// `spec.jobId` over gRPC and 400 over JSON, and no answer repeats the offered id; 63 bytes
/// are accepted. It catches an id that reaches the store unchecked, and one echoed back.
#[tokio::test]
async fn the_id_grammar_is_enforced() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let minted = writes.submit(Via::Grpc, &wire("")).await.unwrap();
    assert_eq!(minted.id.len(), 32);
    assert!(minted.id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let long = "a".repeat(64);
    for bad in [long.as_str(), "a b", "a/b", "é"] {
        let status = writes.submit_grpc(&wire(bad)).await.unwrap_err();
        assert_eq!(
            (status.code(), status.message()),
            (Code::InvalidArgument, "field: spec.jobId"),
            "{bad:?}"
        );
        let (http, body) = writes.submit(Via::Json, &wire(bad)).await.unwrap_err();
        assert_eq!(http, StatusCode::BAD_REQUEST, "{bad:?}");
        assert_eq!(body["error"], "invalid-argument", "{bad:?}");
        assert!(!body.to_string().contains(bad), "{bad:?}: {body}");
    }
    assert_eq!(rows(&writes.jobs), 1);
    let longest = "a".repeat(63);
    let made = writes.submit(Via::Json, &wire(&longest)).await.unwrap();
    assert_eq!(made.id, longest);
}

/// Asserts that an image named by a tag alone is refused `INVALID_ARGUMENT` naming
/// `spec.image` (400 with that field), and the same image pinned by digest is accepted. It
/// catches a tag let through, which would run whatever the tag names when the job starts.
#[tokio::test]
async fn a_tag_only_image_is_refused() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let tagged = v1::JobSpec {
        image: "registry.example/app:latest".to_owned(),
        ..wire("job-t")
    };
    let status = writes.submit_grpc(&tagged).await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::InvalidArgument, "field: spec.image")
    );
    let (http, body) = writes.submit(Via::Json, &tagged).await.unwrap_err();
    assert_eq!(
        (http, &body["field"]),
        (StatusCode::BAD_REQUEST, &json!("spec.image"))
    );
    assert_eq!(rows(&writes.jobs), 0);
    let pinned = v1::JobSpec {
        image: format!("registry.example/app:latest@sha256:{}", "c".repeat(64)),
        ..wire("job-t")
    };
    let made = writes.submit(Via::Json, &pinned).await.unwrap();
    assert_eq!(made.spec.unwrap().image, pinned.image);
}

/// Asserts that a port refusal names the entry and its field (`spec.ports[1].hostPort` for a
/// host port given twice), over gRPC and over JSON. It catches a refusal that names the list
/// and not the port at fault.
#[tokio::test]
async fn a_port_refusal_names_the_entry() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let forward = |host_port| v1::PortForward {
        host_port,
        container_port: 80,
    };
    let spec = v1::JobSpec {
        ports: vec![forward(8080), forward(8080)],
        ..wire("job-p")
    };
    let status = writes.submit_grpc(&spec).await.unwrap_err();
    assert_eq!(status.message(), "field: spec.ports[1].hostPort");
    let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
    assert_eq!(
        (http, &body["field"]),
        (StatusCode::BAD_REQUEST, &json!("spec.ports[1].hostPort"))
    );
}

/// Asserts that the answer of a submit shows the job as it was recorded, over both
/// transports: every field as given (the ISA level and the accelerator too), except the
/// files' contents, which no answer carries, and `aarch64`, recorded as `arm64`. It catches a
/// field the record keeps and the answer drops, and a file's secret contents in an answer.
#[tokio::test]
async fn the_answer_shows_the_job_as_recorded() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    for (via, named) in [(Via::Grpc, "full-g"), (Via::Json, "full-j")] {
        let given = v1::JobSpec {
            needs: Some(v1::Needs {
                cpu_millis: 4000,
                memory_bytes: 8 << 30,
                disk_bytes: Some(1 << 30),
                gpus: 1,
                gpu_class: "nvidia-a100".to_owned(),
                whole_host: false,
            }),
            constraints: Some(v1::Constraints {
                os: "linux".to_owned(),
                arch: "aarch64".to_owned(),
                exec: v1::ExecMode::Container.into(),
                isa_level: "armv8.2-a".to_owned(),
                accelerator: Some(v1::Accelerator {
                    vendor: Some("nvidia".to_owned()),
                    model: Some("nvidia-a100".to_owned()),
                    count: Some(1),
                    memory_per_device_gib: Some(80),
                    interconnect: Some("nvlink>=8".to_owned()),
                    form_factor: Some("sxm".to_owned()),
                }),
            }),
            priority: 3,
            ..conflicted()
        };
        let given = v1::JobSpec {
            job_id: named.to_owned(),
            ..given
        };
        let made = writes.submit(via, &given).await.unwrap();
        let mut shown = given.clone();
        shown.files[0].contents = String::new();
        shown.constraints.as_mut().unwrap().arch = "arm64".to_owned();
        assert_eq!(made.spec.unwrap(), shown, "{via:?}");
    }
}

/// Asserts that a request above 64 KiB is refused on both transports: over gRPC it is not
/// decoded (`OUT_OF_RANGE`), over JSON the gateway answers 413 `request-too-large`; nothing
/// is recorded. It catches a gRPC service left at the library's 4 MiB default.
#[tokio::test]
async fn a_request_above_64_kib_is_refused() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let big = v1::JobSpec {
        env: HashMap::from([("BIG".to_owned(), "x".repeat(70 * 1024))]),
        ..wire("job-big")
    };
    let status = writes.submit_grpc(&big).await.unwrap_err();
    assert_eq!(status.code(), Code::OutOfRange);
    let (http, body) = writes.submit(Via::Json, &big).await.unwrap_err();
    assert_eq!(
        (http, body),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({ "error": "request-too-large" })
        )
    );
    assert_eq!(rows(&writes.jobs), 0);
}
