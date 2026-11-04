//! The router over a stub service, request by request through tower `oneshot`: the routes,
//! the JSON mapping, the decoding of paths, queries and bodies, the limits and the answers
//! that are not a call's response. No socket is opened.

mod common;

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use js_api_gateway::{BODY_LIMIT, ROUTES, Verb, router};
use js_proto::refusal::refusal_status;
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    CancelJobRequest, CordonHostRequest, GetJobRequest, GetJobResponse, GetResultRequest,
    GetResultResponse, GetUsageRequest, JobSpec, JobState, ListJobsRequest, ListJobsResponse,
    RefusalCode, SubmitJobRequest,
};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, Value};
use serde_json::json;
use tokio::sync::oneshot;
use tonic::codegen::tokio_stream;
use tonic::{Code, Request, Status};

use common::{BIG, Stub, invalid, map, send};

/// The pool of the job service, with the options.
fn pool() -> DescriptorPool {
    DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// A stub and the router over it.
fn serve() -> (Arc<Stub>, axum::Router) {
    let stub = Arc::new(Stub::default());
    let router = router(stub.clone());
    (stub, router)
}

/// `template` with every `{variable}` replaced by `x-1`, a valid id of every grammar.
fn fill(template: &str) -> String {
    let mut path = String::new();
    let mut rest = template;
    while let Some((before, after)) = rest.split_once('{') {
        path.push_str(before);
        let (_, tail) = after.split_once('}').expect("a variable closes");
        path.push_str("x-1");
        rest = tail;
    }
    path.push_str(rest);
    path
}

/// The (method, verb, path) of every method of the job service that carries a
/// `google.api.http` option, read with the reflection API alone, not with the crate's
/// annotation module.
fn annotated_methods() -> Vec<(String, &'static str, String)> {
    let pool = pool();
    let http = pool
        .get_extension_by_name("google.api.http")
        .expect("the descriptor set holds the option");
    let service = pool
        .get_service_by_name("jobscheduler.v1.JobService")
        .expect("the service is defined");
    let mut methods = Vec::new();
    for method in service.methods() {
        let options = method.options();
        if !options.has_extension(&http) {
            continue;
        }
        let rule = options.get_extension(&http);
        let rule = rule.as_message().expect("the option is an HttpRule");
        let (verb, field) = if rule.has_field_by_name("get") {
            ("GET", "get")
        } else {
            ("POST", "post")
        };
        let path = rule
            .get_field_by_name(field)
            .expect("the pattern is set")
            .as_str()
            .expect("the pattern is text")
            .to_owned();
        methods.push((method.name().to_owned(), verb, path));
    }
    methods
}

/// Asserts that `ROUTES` is exactly the annotated methods of the descriptor (every method of
/// the service is annotated), in order, and that a request built from each route's template
/// reaches the stub's handler of that method alone, with each path variable in the request
/// field it names. Catches a skipped method, a route bound to another method, and a mangled
/// template such as `{job_id}:cancel`.
#[tokio::test]
async fn every_annotated_rpc_is_routed() {
    let annotated = annotated_methods();
    let service_methods = pool()
        .get_service_by_name("jobscheduler.v1.JobService")
        .expect("the service is defined")
        .methods()
        .len();
    assert_eq!(annotated.len(), service_methods);
    let routes: Vec<(String, &'static str, String)> = ROUTES
        .iter()
        .map(|route| {
            (
                route.rpc.name().to_owned(),
                route.verb.as_str(),
                route.template.to_owned(),
            )
        })
        .collect();
    assert_eq!(routes, annotated);

    for route in ROUTES {
        let (stub, router) = serve();
        let path = fill(route.template);
        let body = if route.body { "{}" } else { "" };
        let reply = send(&router, route.verb.as_str(), &path, body).await;
        assert_eq!(reply.status.as_u16(), route.status, "{path}");
        assert_eq!(stub.rpcs(), [route.rpc.name()], "{path}");
        let input = pool()
            .get_message_by_name(route.input)
            .expect("the request message is defined");
        let request = DynamicMessage::decode(input, stub.calls()[0].request.as_slice())
            .expect("the request decodes");
        let template = route.template;
        for variable in template
            .split('{')
            .skip(1)
            .map(|part| part.split('}').next())
        {
            let name = variable.expect("a variable closes");
            assert_eq!(
                request.get_field_by_name(name).as_deref(),
                Some(&Value::String("x-1".to_owned())),
                "{template}: {name}"
            );
        }
    }
}

/// Asserts that a test descriptor's method without the option (`gatewaytest.Good.Hidden`)
/// gets no route while its annotated methods do, and that on the real router paths and
/// methods outside `ROUTES` answer 404 or 405 and reach no method: the lease and agent
/// routes, a path below a job that no template has, a method's name as a path (the gRPC
/// form and REST forms), and every method of HTTP but the annotated one on each route's
/// path (HEAD, OPTIONS, TRACE and CONNECT among them, and GET on the cancel route's path,
/// which is that route's and not a job id with a colon). Catches a fallback route such as
/// `POST /<service>/<method>`.
#[tokio::test]
async fn an_unannotated_rpc_is_not_exposed() {
    let pool = common::fixture("gatewaytest/good.proto");
    let service = pool
        .get_service_by_name("gatewaytest.Good")
        .expect("the test service is defined");
    assert!(service.methods().any(|method| method.name() == "Hidden"));
    let routed: Vec<String> = js_api_gateway::annotation::routes(&pool, "gatewaytest.Good")
        .expect("the test options pass")
        .into_iter()
        .map(|route| route.rpc)
        .collect();
    assert_eq!(routed, ["Read", "List", "Act", "Other"]);

    let (stub, router) = serve();
    let outside = [
        ("GET", "/v1/leases"),
        ("POST", "/v1/leases"),
        ("POST", "/v1/agent/hello"),
        ("GET", "/healthz"),
        ("GET", "/status"),
        ("GET", "/jobscheduler.v1.JobService/GetJob"),
        ("POST", "/jobscheduler.v1.JobService/GetJob"),
        ("GET", "/v1/GetJob"),
        ("GET", "/v1/Hidden"),
        ("POST", "/gatewaytest.Good/Hidden"),
        ("POST", "/v1/jobs/job-1/hidden"),
        ("POST", "/v1/jobs:submit"),
        ("GET", "/v1/jobs/x/anything-else"),
        ("GET", "/v1/jobs/x/result/more"),
        ("POST", "/v1/hosts/host-a:drain"),
    ];
    for (method, path) in outside {
        let reply = send(&router, method, path, "").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    for route in ROUTES {
        let path = fill(route.template);
        let methods = [
            "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
        ];
        for method in methods {
            let served = ROUTES
                .iter()
                .any(|other| other.verb.as_str() == method && fill(other.template) == path);
            if served {
                continue;
            }
            let reply = send(&router, method, &path, "").await;
            assert!(
                [StatusCode::NOT_FOUND, StatusCode::METHOD_NOT_ALLOWED].contains(&reply.status),
                "{method} {path}: {}",
                reply.status
            );
        }
    }
    assert_eq!(stub.rpcs(), Vec::<&str>::new());
}

/// Decodes a JSON body as the message `name` by the proto3 JSON mapping and converts it to
/// `M`.
fn from_json<M: Message + Default>(name: &str, body: &[u8]) -> M {
    let descriptor = pool()
        .get_message_by_name(name)
        .expect("the message is defined");
    let json: serde_json::Value = serde_json::from_slice(body).expect("the body is JSON");
    DynamicMessage::deserialize(descriptor, json)
        .expect("the body decodes")
        .transcode_to::<M>()
        .expect("the message converts")
}

/// Asserts that `GetJob`, `ListJobs` and `GetResult` answer over HTTP the message the
/// service's trait answers, field for field, for messages with every field set, among them
/// 64-bit fields above 2^53 and enums; that those are written as decimal strings and enum
/// names; and that each answer comes from one call of the trait. Catches a dropped, renamed
/// or truncated field and an answer made from anything but the trait.
#[tokio::test]
async fn json_and_grpc_return_the_same_job() {
    let (stub, router) = serve();
    let direct = stub.clone();

    let grpc = direct
        .get_job(Request::new(GetJobRequest {
            job_id: "job-7".to_owned(),
        }))
        .await
        .expect("the stub answers")
        .into_inner();
    let reply = send(&router, "GET", "/v1/jobs/job-7", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers.get("content-type").map(|v| v.as_bytes()),
        Some(&b"application/json"[..])
    );
    assert_eq!(
        from_json::<GetJobResponse>("jobscheduler.v1.GetJobResponse", &reply.body),
        grpc
    );
    let json = reply.json();
    assert_eq!(json["job"]["times"]["queuedAtMs"], json!(BIG.to_string()));
    assert_eq!(
        json["job"]["spec"]["needs"]["memoryBytes"],
        json!("9007199254740993")
    );
    assert_eq!(json["job"]["state"], json!("JOB_STATE_FAILED_TO_START"));
    assert_eq!(
        json["job"]["spec"]["network"],
        json!("NETWORK_LEVEL_ALLOW_LIST")
    );
    assert_eq!(json["job"]["spec"]["priority"], json!(-7));
    assert_eq!(
        json["job"].get("host_id"),
        None,
        "proto names are not written"
    );

    let grpc = direct
        .list_jobs(Request::new(ListJobsRequest::default()))
        .await
        .expect("the stub answers")
        .into_inner();
    let reply = send(&router, "GET", "/v1/jobs", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        from_json::<ListJobsResponse>("jobscheduler.v1.ListJobsResponse", &reply.body),
        grpc
    );
    assert_eq!(reply.json()["nextPageToken"], json!("page-2"));

    let grpc = direct
        .get_result(Request::new(GetResultRequest {
            job_id: "job-1".to_owned(),
        }))
        .await
        .expect("the stub answers")
        .into_inner();
    let reply = send(&router, "GET", "/v1/jobs/job-1/result", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        from_json::<GetResultResponse>("jobscheduler.v1.GetResultResponse", &reply.body),
        grpc
    );
    let json = reply.json();
    assert_eq!(
        json["result"]["usage"]["cpuUsec"],
        json!("9007199254740993")
    );
    assert_eq!(json["result"]["reason"], json!("RESULT_REASON_MEMORY"));
    assert_eq!(json["result"]["stdoutTail"], json!("AP9vdXQ="));

    assert_eq!(
        stub.rpcs(),
        [
            "GetJob",
            "GetJob",
            "ListJobs",
            "ListJobs",
            "GetResult",
            "GetResult"
        ]
    );
}

/// Asserts that a submit answers 201 with the job, and a repeated identical submit 201 with
/// the same body, and that every other route answers a success 200: the status each method's
/// `http_success_status` option names (`SubmitJob` alone sets 201). Catches a success
/// status that is not the option's, and a repeat answered differently from the first submit.
#[tokio::test]
async fn submit_answers_201_and_every_other_call_200() {
    let (stub, router) = serve();
    let body = r#"{"spec":{"jobId":"job-1"}}"#;
    let first = send(&router, "POST", "/v1/jobs", body).await;
    let second = send(&router, "POST", "/v1/jobs", body).await;
    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(second.status, StatusCode::CREATED);
    assert_eq!(first.body, second.body);
    assert_eq!(first.json()["job"]["id"], json!("job-1"));
    assert_eq!(stub.rpcs(), ["SubmitJob", "SubmitJob"]);

    let statuses: Vec<(&str, u16)> = ROUTES
        .iter()
        .map(|route| (route.rpc.name(), route.status))
        .collect();
    let expected: Vec<(&str, u16)> = ROUTES
        .iter()
        .map(|route| match route.rpc.name() {
            "SubmitJob" => ("SubmitJob", 201),
            other => (other, 200),
        })
        .collect();
    assert_eq!(statuses, expected);
    for route in ROUTES
        .iter()
        .filter(|route| route.rpc.name() != "SubmitJob")
    {
        let (_, router) = serve();
        let body = if route.body { "{}" } else { "" };
        let reply = send(&router, route.verb.as_str(), &fill(route.template), body).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", route.template);
    }
}

/// The 400 body of a bad id in `field`.
fn bad_id(field: &str) -> serde_json::Value {
    invalid(field, "the field is not a valid id")
}

/// Asserts that a job id outside its grammar (a character outside letters, digits, dot,
/// underscore and dash, a percent-escaped slash among them; 64 bytes; an empty path
/// segment) is a 400 naming `jobId` that repeats no value and calls nothing, in the path,
/// in a query and as `spec.jobId` in a submit's body; that an empty host id in a path is a
/// 400 naming `hostId`; and that job ids in the grammar (an upper-case one, as the design's
/// letters allow, and a 63-byte one) and any non-empty host id reach the service. Catches
/// an id that reaches the service, a 400 with no field, and an echoed value.
#[tokio::test]
async fn bad_job_id_is_a_400_naming_the_field() {
    let (stub, router) = serve();
    let long = "a".repeat(64);
    let refused = [
        ("GET", "/v1/jobs/bad~id".to_owned(), "jobId"),
        ("GET", format!("/v1/jobs/{long}"), "jobId"),
        ("GET", "/v1/jobs//result".to_owned(), "jobId"),
        ("GET", "/v1/jobs/bad*id/logs".to_owned(), "jobId"),
        ("GET", "/v1/jobs/bad%2Fid".to_owned(), "jobId"),
        ("GET", "/v1/jobs/%2561".to_owned(), "jobId"),
        ("GET", "/v1/jobs/".to_owned(), "jobId"),
        ("POST", "/v1/jobs/bad~id:cancel".to_owned(), "jobId"),
        ("POST", "/v1/hosts/:uncordon".to_owned(), "hostId"),
        ("POST", "/v1/hosts/:cordon".to_owned(), "hostId"),
        ("GET", "/v1/usage?job_id=bad~id".to_owned(), "jobId"),
    ];
    for (method, path, field) in &refused {
        let reply = send(&router, method, path, "").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{method} {path}");
        assert_eq!(reply.json(), bad_id(field), "{method} {path}");
    }
    let body = json!({ "spec": { "jobId": "bad~id", "image": "x" } }).to_string();
    let reply = send(&router, "POST", "/v1/jobs", body).await;
    assert_eq!(reply.json(), bad_id("spec.jobId"));
    assert_eq!(stub.rpcs(), Vec::<&str>::new());

    let max = "b".repeat(63);
    let reply = send(&router, "GET", &format!("/v1/jobs/{max}"), "").await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&router, "GET", "/v1/jobs/Job-A.1_x", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&router, "GET", "/v1/usage?hostId=Host_A&jobId=Job-A", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&router, "POST", "/v1/hosts/Host_A:uncordon", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    let calls = stub.calls();
    let got: Vec<String> = calls[..2]
        .iter()
        .map(|call| {
            GetJobRequest::decode(call.request.as_slice())
                .expect("the request decodes")
                .job_id
        })
        .collect();
    assert_eq!(got, [max, "Job-A.1_x".to_owned()]);
    assert_eq!(
        GetUsageRequest::decode(calls[2].request.as_slice()).expect("the request decodes"),
        GetUsageRequest {
            host_id: "Host_A".to_owned(),
            job_id: "Job-A".to_owned(),
            ..GetUsageRequest::default()
        }
    );
}

/// Asserts that a submit body with a field the request does not have is a 400 naming it (at
/// the top, and inside the job as `spec.<name>` after a field that decodes), and that the
/// service is not called. Catches unknown fields that are dropped.
#[tokio::test]
async fn submit_body_unknown_field_is_a_400() {
    let (stub, router) = serve();
    let cases = [
        (json!({ "spec": {}, "bogus": 1 }), "bogus"),
        (json!({ "spec": { "image": "x", "zzz": true } }), "spec.zzz"),
        (
            json!({ "spec": { "needs": { "cpuMillis": "1", "cores": 2 } } }),
            "spec.needs.cores",
        ),
    ];
    for (body, field) in cases {
        let reply = send(&router, "POST", "/v1/jobs", body.to_string()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            reply.json(),
            invalid(
                field,
                "the field is unknown, or does not hold a value of its type"
            ),
            "{body}"
        );
    }
    assert_eq!(stub.rpcs(), Vec::<&str>::new());
}

/// The text of a submit body before its command.
const HEAD: &str = r#"{"spec":{"jobId":"job-1","command":""#;

/// The text of a submit body after its command.
const TAIL: &str = r#""}}"#;

/// A submit body of exactly `size` bytes of valid JSON.
fn submit_body(size: usize) -> String {
    let body = format!("{HEAD}{}{TAIL}", "x".repeat(size - HEAD.len() - TAIL.len()));
    assert_eq!(body.len(), size);
    body
}

/// Asserts that a body of 65537 bytes (valid JSON) is a 413 `request-too-large` that calls
/// nothing, that one of exactly 65536 bytes is decoded and reaches the service whole, and
/// that the limit is the lease server's 64 KiB. Catches an off-by-one and a missing limit.
#[tokio::test]
async fn body_over_64_kib_is_413_and_not_decoded() {
    assert_eq!(BODY_LIMIT, 65536);
    let (stub, router) = serve();
    let reply = send(&router, "POST", "/v1/jobs", submit_body(BODY_LIMIT + 1)).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.json(), json!({ "error": "request-too-large" }));
    assert_eq!(stub.rpcs(), Vec::<&str>::new());

    let reply = send(&router, "POST", "/v1/jobs", submit_body(BODY_LIMIT)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let request: SubmitJobRequest = stub.only_request();
    let spec = request.spec.expect("the job is submitted");
    assert_eq!(spec.job_id, "job-1");
    assert_eq!(spec.command.len(), BODY_LIMIT - HEAD.len() - TAIL.len());
}

/// The golden line of an answer: `<status> <code>`, then ` <field>` when it names one.
fn line(answer: &js_api_gateway::Answer) -> String {
    match answer.field() {
        Some(field) => format!("{} {} {field}", answer.status().as_u16(), answer.code()),
        None => format!("{} {}", answer.status().as_u16(), answer.code()),
    }
}

/// Asserts that every value of `RefusalCode`, enumerated from the descriptor set, maps to
/// the status, code and field of `golden/refusals.txt`, whether mapped directly or read
/// from the status a server answers a refused submit with. Catches a mapping that changes,
/// and a refusal value added to the definition without a golden line.
#[test]
fn refusal_to_status_table_matches_golden() {
    let pool = pool();
    let refusals = pool
        .get_enum_by_name("jobscheduler.v1.RefusalCode")
        .expect("RefusalCode is defined");
    let mut lines = Vec::new();
    for value in refusals.values() {
        let code = RefusalCode::try_from(value.number()).expect("the generated enum has it");
        let direct = js_api_gateway::Answer::of_refusal(code);
        let carried = js_api_gateway::Answer::of_status(&refusal_status(code, "a detail"));
        assert_eq!(carried, direct, "{}", value.name());
        lines.push(format!("{} = {}", value.name(), line(&direct)));
    }
    let golden: Vec<&str> = include_str!("golden/refusals.txt")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect();
    assert_eq!(lines, golden);
}

/// The messages each gRPC code is tried with in the status table.
const MESSAGES: [&str; 6] = [
    "unknown-job",
    "expired",
    "not-finished",
    "state-write-failed",
    "field: spec.image",
    "something else",
];

/// Asserts that every gRPC code with each message of [`MESSAGES`] maps to the line of
/// `golden/statuses.txt`. Catches a status row that changes unnoticed.
#[test]
fn status_table_matches_golden() {
    let mut lines = Vec::new();
    for number in 0..=16 {
        let code = Code::from(number);
        for message in MESSAGES {
            let answer = js_api_gateway::Answer::of_status(&Status::new(code, message));
            lines.push(format!("{code:?} {message:?} = {}", line(&answer)));
        }
    }
    let golden: Vec<&str> = include_str!("golden/statuses.txt")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect();
    assert_eq!(lines, golden);
}

/// Asserts that the statuses of one gRPC code that differ in message, or in a refusal they
/// carry, each select their own row, through the router: `NOT_FOUND` with `unknown-job`
/// (404) and `expired` (410), `FAILED_PRECONDITION` with `not-finished` (409) and with a
/// `no-capable-host` refusal (422), `INTERNAL` with `state-write-failed` (500
/// `state-write-failed`) and with another message (500 `internal`), and the rest of the
/// table. Catches two statuses of one code collapsing into one row, and a message that is
/// ignored.
#[tokio::test]
async fn a_status_message_selects_the_row() {
    let wall = "spec.wallLimitSeconds";
    let rows = [
        (
            Status::not_found("unknown-job"),
            404,
            json!({ "error": "unknown-job" }),
        ),
        (
            Status::not_found("expired"),
            410,
            json!({ "error": "expired" }),
        ),
        (
            Status::failed_precondition("not-finished"),
            409,
            json!({ "error": "not-finished" }),
        ),
        (
            refusal_status(RefusalCode::NoCapableHost, "no host has 8 GPUs"),
            422,
            json!({ "error": "no-capable-host" }),
        ),
        (
            refusal_status(RefusalCode::JobIdConflict, "differs"),
            409,
            json!({ "error": "job-id-conflict" }),
        ),
        (
            refusal_status(RefusalCode::WallTimeNotPositive, "none"),
            400,
            json!({
                "error": "wall-time-not-positive",
                "field": wall,
                "reason": "the job has no wall limit, or one of 0",
            }),
        ),
        (
            refusal_status(RefusalCode::WallTimeAboveCeiling, "too long"),
            400,
            json!({
                "error": "wall-time-above-ceiling",
                "field": wall,
                "reason": "the job's wall limit is above the service's ceiling",
            }),
        ),
        (
            Status::internal("state-write-failed"),
            500,
            json!({ "error": "state-write-failed" }),
        ),
        (
            Status::internal("disk full"),
            500,
            json!({ "error": "internal" }),
        ),
        (
            Status::unimplemented("later"),
            501,
            json!({ "error": "unimplemented" }),
        ),
        (
            Status::invalid_argument("field: spec.image"),
            400,
            invalid(
                "spec.image",
                "the service refused an argument of the request",
            ),
        ),
        (
            Status::invalid_argument("bad"),
            400,
            json!({
                "error": "invalid-argument",
                "reason": "the service refused an argument of the request",
            }),
        ),
        (
            Status::not_found("unknown-host"),
            500,
            json!({ "error": "internal" }),
        ),
        (
            Status::unavailable("try later"),
            500,
            json!({ "error": "internal" }),
        ),
    ];
    for (status, code, body) in rows {
        let shown = format!("{:?} {}", status.code(), status.message());
        let stub = Arc::new(Stub::failing(status));
        let router = router(stub.clone());
        let reply = send(&router, "POST", "/v1/jobs", r#"{"spec":{}}"#).await;
        assert_eq!(reply.status.as_u16(), code, "{shown}");
        assert_eq!(reply.json(), body, "{shown}");
        assert_eq!(stub.rpcs(), ["SubmitJob"], "{shown}");
    }
}

/// Asserts that `POST /v1/jobs/{id}:cancel` answers 200 twice with the same cancelled job,
/// calling `CancelJob` each time with the id (with no body, and with an empty object); that
/// a body naming the id the path sets is a 400; and that `DELETE` on the job's path is a
/// 405 whose `Allow` is `GET`, as is `DELETE` on `/v1/jobs` (`POST, GET`). Catches a cancel
/// under another method or path.
#[tokio::test]
async fn cancel_is_a_post_with_a_custom_verb_and_is_idempotent() {
    let cancel = ROUTES
        .iter()
        .find(|route| route.rpc.name() == "CancelJob")
        .expect("CancelJob has a route");
    assert_eq!(
        (cancel.verb, cancel.template, cancel.body),
        (Verb::Post, "/v1/jobs/{job_id}:cancel", true)
    );
    let (stub, router) = serve();
    let first = send(&router, "POST", "/v1/jobs/job-1:cancel", "").await;
    let second = send(&router, "POST", "/v1/jobs/job-1:cancel", "{}").await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(first.body, second.body);
    assert_eq!(first.json()["job"]["state"], json!("JOB_STATE_CANCELLED"));
    let requests: Vec<CancelJobRequest> = stub
        .calls()
        .iter()
        .map(|call| CancelJobRequest::decode(call.request.as_slice()).expect("it decodes"))
        .collect();
    let expected = CancelJobRequest {
        job_id: "job-1".to_owned(),
    };
    assert_eq!(requests, [expected.clone(), expected]);

    let reply = send(
        &router,
        "POST",
        "/v1/jobs/job-1:cancel",
        r#"{"jobId":"job-2"}"#,
    )
    .await;
    assert_eq!(
        reply.json(),
        invalid("jobId", "the field is set by the path")
    );

    let reply = send(&router, "DELETE", "/v1/jobs/job-1", "").await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(reply.json(), json!({ "error": "method-not-allowed" }));
    assert_eq!(
        reply.headers.get("allow").map(|v| v.as_bytes()),
        Some(&b"GET"[..])
    );
    let reply = send(&router, "DELETE", "/v1/jobs", "").await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        reply.headers.get("allow").map(|v| v.as_bytes()),
        Some(&b"POST, GET"[..])
    );
    assert_eq!(stub.rpcs(), ["CancelJob", "CancelJob"]);
}

/// Asserts that an internal failure, an unknown one and a status of a code the table does
/// not list are each a 500 whose whole body is `{"error":"internal"}`: the distinctive
/// text of the cause is not in it. Catches an echoed cause.
#[tokio::test]
async fn internal_error_does_not_leak_its_cause() {
    let secret = "db password hunter2 at /var/lib/store";
    for status in [
        Status::internal(secret),
        Status::unknown(secret),
        Status::data_loss(secret),
    ] {
        let router = router(Arc::new(Stub::failing(status)));
        let reply = send(&router, "GET", "/v1/jobs/job-1", "").await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, br#"{"error":"internal"}"#);
    }
}

/// Asserts that a query sets the fields of `ListJobs` by JSON name or proto name: a
/// repeated enum by repeating it, a map entry as `labels[key]`, a 64-bit value as text, a
/// percent-encoded value and `+` as a space; and that `GetUsage` takes its filters so too.
/// Catches a query field that is dropped or mangled.
#[tokio::test]
async fn a_query_sets_the_fields_of_a_read() {
    let (stub, router) = serve();
    let query = "states=JOB_STATE_QUEUED&states=JOB_STATE_RUNNING&labels[team]=ml&\
                 labels%5Bkind%5D=a+b&queuedAfterMs=9007199254740993&queued_before_ms=7&\
                 pageSize=10&page_token=t%2F1&&";
    let reply = send(&router, "GET", &format!("/v1/jobs?{query}"), "").await;
    assert_eq!(reply.status, StatusCode::OK);
    let request: ListJobsRequest = stub.only_request();
    assert_eq!(
        request,
        ListJobsRequest {
            states: vec![JobState::Queued.into(), JobState::Running.into()],
            labels: map(&[("team", "ml"), ("kind", "a b")]),
            queued_after_ms: Some(BIG),
            queued_before_ms: Some(7),
            page_size: 10,
            page_token: "t/1".to_owned(),
        }
    );
}

/// Asserts the 400s of a query and the field each names: an unknown parameter, a map
/// without a key, a key on a field that is not a map, a field and a map entry given twice
/// (also when two spellings decode to the same name or key), a value of the wrong kind,
/// malformed percent escapes (trailing, incomplete, not hexadecimal) in a name and in a
/// value, text that is not UTF-8, a query on a route whose body is the request, and a body
/// on a route that takes none. Catches a malformed query that reaches the service.
#[tokio::test]
async fn a_malformed_query_is_a_400() {
    let (stub, router) = serve();
    let twice = "the field is set twice";
    let malformed = "a map field is set as name[key]=value, any other field as name=value";
    let unknown = "the request has no such field";
    let wrong = "the field is unknown, or does not hold a value of its type";
    let text = "the text has a malformed percent escape or does not decode as UTF-8";
    let cases = [
        ("GET", "/v1/jobs?bogus=1", invalid("bogus", unknown)),
        ("GET", "/v1/jobs?labels=x", invalid("labels", malformed)),
        (
            "GET",
            "/v1/jobs?pageSize[a]=1",
            invalid("pageSize[a]", malformed),
        ),
        (
            "GET",
            "/v1/jobs?states[a]=JOB_STATE_QUEUED",
            invalid("states[a]", malformed),
        ),
        ("GET", "/v1/jobs?nope[a]=1", invalid("nope[a]", unknown)),
        (
            "GET",
            "/v1/jobs?pageSize=1&page_size=2",
            invalid("page_size", twice),
        ),
        (
            "GET",
            "/v1/jobs?pageSize=1&page%53ize=2",
            invalid("pageSize", twice),
        ),
        (
            "GET",
            "/v1/jobs?labels[a]=1&labels[a]=2",
            invalid("labels[a]", twice),
        ),
        (
            "GET",
            "/v1/jobs?labels[a]=1&labels%5Ba%5D=2",
            invalid("labels[a]", twice),
        ),
        ("GET", "/v1/jobs?pageSize=ten", invalid("pageSize", wrong)),
        (
            "GET",
            "/v1/jobs?states=JOB_STATE_NONE",
            invalid("states", wrong),
        ),
        (
            "GET",
            "/v1/jobs?page_token=%FF",
            invalid("page_token", text),
        ),
        ("GET", "/v1/jobs?pageToken=%zz", invalid("pageToken", text)),
        ("GET", "/v1/jobs?pageToken=%4", invalid("pageToken", text)),
        ("GET", "/v1/jobs?pageToken=a%", invalid("pageToken", text)),
        (
            "GET",
            "/v1/jobs?%zz=1",
            json!({ "error": "invalid-argument", "reason": text }),
        ),
        (
            "POST",
            "/v1/jobs?spec=1",
            invalid(
                "spec",
                "this route takes its fields in the body, not in the query",
            ),
        ),
    ];
    for (method, uri, body) in cases {
        let reply = send(&router, method, uri, "").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(reply.json(), body, "{uri}");
    }
    let reply = send(&router, "GET", "/v1/jobs", "{}").await;
    assert_eq!(
        reply.json(),
        json!({
            "error": "invalid-argument",
            "reason": "this route takes no body; its fields are set by the path and the query",
        })
    );
    assert_eq!(stub.rpcs(), Vec::<&str>::new());
}

/// Asserts what a query's names and keys decode to: a percent-encoded field name, a
/// parameter without `=` (an empty value), an empty map key and a map key that holds
/// brackets. Catches a query boundary read as another field or key.
#[tokio::test]
async fn query_names_and_keys_decode_once() {
    let (stub, router) = serve();
    let uri = "/v1/jobs?page%53ize=5&pageToken&labels[]=empty&labels[a[b]]=nested";
    let reply = send(&router, "GET", uri, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        stub.only_request::<ListJobsRequest>(),
        ListJobsRequest {
            page_size: 5,
            labels: map(&[("", "empty"), ("a[b]", "nested")]),
            ..ListJobsRequest::default()
        }
    );
}

/// Asserts that a path variable is percent-decoded exactly once before its checks: `%61`
/// is the job id `a` and `host%5Fa` the host id `host_a`; `%2561` decodes to `%61`, which
/// is no job id; and a malformed escape or bytes that are not UTF-8 are a 400 naming the
/// field. Catches a path read as written, or decoded twice.
#[tokio::test]
async fn a_path_variable_is_decoded_once() {
    let (stub, router) = serve();
    let reply = send(&router, "GET", "/v1/jobs/%61", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["job"]["id"], json!("a"));
    let reply = send(&router, "POST", "/v1/hosts/host%5Fa:cordon", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    let calls = stub.calls();
    assert_eq!(
        CordonHostRequest::decode(calls[1].request.as_slice()).expect("it decodes"),
        CordonHostRequest {
            host_id: "host_a".to_owned(),
            cordon_secs: None,
        }
    );
    let text = "the text has a malformed percent escape or does not decode as UTF-8";
    for path in ["/v1/jobs/%FF", "/v1/jobs/a%zz", "/v1/jobs/a%"] {
        let reply = send(&router, "GET", path, "").await;
        assert_eq!(reply.json(), invalid("jobId", text), "{path}");
    }
    assert_eq!(stub.rpcs(), ["GetJob", "CordonHost"]);
}

/// Asserts that a body or query that names a field the path sets is a 400 naming it,
/// whatever it holds: a different id, the empty default, `null`, under the JSON name or the
/// proto name, and an empty query value; and that a body naming only other fields is
/// taken. Catches a path field silently overwriting what the request gave.
#[tokio::test]
async fn a_field_the_path_sets_is_given_once() {
    let (stub, router) = serve();
    let by_path = invalid("jobId", "the field is set by the path");
    for body in [
        r#"{"jobId":"job-2"}"#,
        r#"{"jobId":""}"#,
        r#"{"job_id":null}"#,
    ] {
        let reply = send(&router, "POST", "/v1/jobs/job-1:cancel", body).await;
        assert_eq!(reply.json(), by_path, "{body}");
    }
    let reply = send(&router, "GET", "/v1/jobs/job-1?jobId=", "").await;
    assert_eq!(reply.json(), by_path);
    assert_eq!(stub.rpcs(), Vec::<&str>::new());
    let reply = send(
        &router,
        "POST",
        "/v1/hosts/host-a:cordon",
        r#"{"cordonSecs":60}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

/// Asserts that an integer field holding a JSON number is taken only as an exact integer:
/// the largest `u64` and a negative `int32` reach the service unchanged, while a number
/// past the `u64` range (which a JSON parser would round to a float), a fraction, an
/// exponent form, an integral number written with a fraction, and a decimal string past the
/// range are each a 400 naming the field, inside a list too. Catches a value silently
/// rounded or saturated.
#[tokio::test]
async fn an_integer_is_taken_exactly_or_refused() {
    let (stub, router) = serve();
    let body = r#"{"spec":{"jobId":"job-1","needs":{"memoryBytes":18446744073709551615},"priority":-2147483648,"network":2}}"#;
    let reply = send(&router, "POST", "/v1/jobs", body).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let spec = stub
        .only_request::<SubmitJobRequest>()
        .spec
        .expect("the job is submitted");
    assert_eq!(spec.needs.map(|needs| needs.memory_bytes), Some(u64::MAX));
    assert_eq!(spec.priority, i32::MIN);
    assert_eq!(spec.network, 2);

    let (stub, router) = serve();
    let wrong = "the field is unknown, or does not hold a value of its type";
    let memory = "spec.needs.memoryBytes";
    let cases = [
        (
            r#"{"spec":{"needs":{"memoryBytes":18446744073709551616}}}"#,
            memory,
        ),
        (r#"{"spec":{"needs":{"memoryBytes":1.5}}}"#, memory),
        (r#"{"spec":{"needs":{"memoryBytes":1e3}}}"#, memory),
        (r#"{"spec":{"needs":{"memoryBytes":2.0}}}"#, memory),
        (
            r#"{"spec":{"needs":{"memoryBytes":"18446744073709551616"}}}"#,
            memory,
        ),
        (
            r#"{"spec":{"ports":[{"hostPort":80},{"hostPort":8.5}]}}"#,
            "spec.ports[1].hostPort",
        ),
    ];
    for (body, field) in cases {
        let reply = send(&router, "POST", "/v1/jobs", body).await;
        assert_eq!(reply.json(), invalid(field, wrong), "{body}");
    }
    assert_eq!(stub.rpcs(), Vec::<&str>::new());
}

/// Asserts the 400s of a body and the field each names, by its JSON-name path: not JSON,
/// JSON that is not an object, an object that gives a member twice (at the top and inside a
/// map), a value of the wrong kind (at the top, inside the job, in a list element, in a
/// message of a list, in a map), an unknown field inside a list's message, a field written
/// by its proto name (named by its JSON name), one field under both its names (inside the
/// job, at the top, and with one of them `null`), a `null` list element, and a body that
/// cannot be read. Catches a malformed body that reaches the service, and a 400 that names
/// the wrong field or none.
#[tokio::test]
async fn a_malformed_body_is_a_400() {
    let (stub, router) = serve();
    let wrong = "the field is unknown, or does not hold a value of its type";
    let twice = "the field is set twice";
    let member = json!({
        "error": "invalid-argument",
        "reason": "an object of the body gives a member twice",
    });
    let cases = [
        (
            "/v1/jobs",
            "{",
            json!({ "error": "invalid-argument", "reason": "the body is not valid JSON" }),
        ),
        (
            "/v1/jobs",
            "[]",
            json!({ "error": "invalid-argument", "reason": "the body is not a JSON object" }),
        ),
        ("/v1/jobs", r#"{"spec":{},"spec":{}}"#, member.clone()),
        (
            "/v1/jobs",
            r#"{"spec":{"labels":{"a":"1","a":"2"}}}"#,
            member,
        ),
        ("/v1/jobs", r#"{"spec":5}"#, invalid("spec", wrong)),
        (
            "/v1/jobs",
            r#"{"spec":{"priority":"high"}}"#,
            invalid("spec.priority", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"args":["a",1]}}"#,
            invalid("spec.args[1]", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"args":[null]}}"#,
            invalid("spec.args[0]", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"ports":[{"hostPort":1},{"hostPort":"x"}]}}"#,
            invalid("spec.ports[1].hostPort", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"ports":[{"bogus":1}]}}"#,
            invalid("spec.ports[0].bogus", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"labels":{"a":1}}}"#,
            invalid("spec.labels", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"wall_limit_seconds":"x"}}"#,
            invalid("spec.wallLimitSeconds", wrong),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"wallLimitSeconds":"1","wall_limit_seconds":"2"}}"#,
            invalid("spec.wallLimitSeconds", twice),
        ),
        (
            "/v1/jobs",
            r#"{"spec":{"jobId":null,"job_id":"job-1"}}"#,
            invalid("spec.jobId", twice),
        ),
        (
            "/v1/hosts/host-a:cordon",
            r#"{"cordonSecs":1,"cordon_secs":2}"#,
            invalid("cordonSecs", twice),
        ),
    ];
    for (path, body, expected) in cases {
        let reply = send(&router, "POST", path, body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.json(), expected, "{body}");
    }
    let broken = Body::from_stream(tokio_stream::iter([
        Ok(Bytes::from_static(b"{")),
        Err(io::Error::other("the peer went away")),
    ]));
    let reply = send(&router, "POST", "/v1/jobs", broken).await;
    assert_eq!(
        reply.json(),
        json!({ "error": "invalid-argument", "reason": "the body could not be read" })
    );
    assert_eq!(stub.rpcs(), Vec::<&str>::new());
}

/// Asserts what `null` and explicit defaults mean in a body, by the proto3 JSON mapping:
/// `null` for a message field leaves it absent, an optional field set to zero is present
/// and zero (not absent), and an absent optional field is absent; and that a body with
/// booleans, negative and unsigned numbers, strings, lists, objects and `null` together is
/// read whole. Catches presence lost between the HTTP body and the call.
#[tokio::test]
async fn null_and_explicit_defaults_keep_their_meaning() {
    let (stub, router) = serve();
    let bodies = [
        r#"{"spec":null}"#,
        r#"{"spec":{"jobId":"job-1","wallLimitSeconds":"0"}}"#,
        r#"{"spec":{"jobId":"job-1"}}"#,
        r#"{"spec":{"jobId":"job-1","args":["a"],"needs":{"wholeHost":true,"gpus":1,"diskBytes":null},"priority":-1,"constraints":null}}"#,
    ];
    for body in bodies {
        let reply = send(&router, "POST", "/v1/jobs", body).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{body}");
    }
    let specs: Vec<Option<JobSpec>> = stub
        .calls()
        .iter()
        .map(|call| {
            SubmitJobRequest::decode(call.request.as_slice())
                .expect("it decodes")
                .spec
        })
        .collect();
    assert_eq!(specs[0], None);
    let wall = |index: usize| specs[index].as_ref().map(|spec| spec.wall_limit_seconds);
    assert_eq!(wall(1), Some(Some(0)));
    assert_eq!(wall(2), Some(None));
    let last = specs[3].clone().expect("the job is submitted");
    assert_eq!(last.args, ["a"]);
    assert_eq!(last.priority, -1);
    assert_eq!(last.constraints, None);
    let needs = last.needs.expect("the needs are given");
    assert_eq!(
        (needs.whole_host, needs.gpus, needs.disk_bytes),
        (true, 1, None)
    );
}

/// A body stream of `frames` chunks of `size` bytes each, counting the chunks it hands out
/// in `taken`.
fn counted(frames: usize, size: usize, taken: Arc<AtomicUsize>) -> Body {
    let chunks = (0..frames).map(move |_| {
        taken.fetch_add(1, Ordering::SeqCst);
        Ok::<_, io::Error>(Bytes::from(vec![b' '; size]))
    });
    Body::from_stream(tokio_stream::iter(chunks))
}

/// Asserts that a body streamed across frames is cut at the limit: four frames of 30000
/// bytes are a 413 after the third frame (no frame after the one that crosses the limit is
/// pulled) and call nothing; an oversized body that is not JSON is a 413 too, not a 400; and
/// a streamed body of exactly 65536 bytes is taken. Catches a limit applied after the body
/// is read whole, and JSON decoded before the limit.
#[tokio::test]
async fn a_streamed_body_stops_at_the_limit() {
    let (stub, router) = serve();
    let taken = Arc::new(AtomicUsize::new(0));
    let reply = send(
        &router,
        "POST",
        "/v1/jobs",
        counted(4, 30_000, taken.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.json(), json!({ "error": "request-too-large" }));
    assert_eq!(taken.load(Ordering::SeqCst), 3);
    let reply = send(&router, "POST", "/v1/jobs", "x".repeat(BODY_LIMIT + 1)).await;
    assert_eq!(reply.json(), json!({ "error": "request-too-large" }));
    assert_eq!(stub.rpcs(), Vec::<&str>::new());

    let half = BODY_LIMIT / 2;
    let body = submit_body(BODY_LIMIT);
    let (first, second) = body.split_at(half);
    let frames = [
        Ok::<_, io::Error>(Bytes::from(first.to_owned())),
        Ok(Bytes::from(second.to_owned())),
    ];
    let reply = send(
        &router,
        "POST",
        "/v1/jobs",
        Body::from_stream(tokio_stream::iter(frames)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(stub.rpcs(), ["SubmitJob"]);
}

/// The wire form of `google.rpc.Status`, to write status details by hand.
#[derive(Clone, PartialEq, Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}

/// A detail under `type_url` holding `value`.
fn detail(type_url: &str, value: Vec<u8>) -> prost_types::Any {
    prost_types::Any {
        type_url: type_url.to_owned(),
        value,
    }
}

/// The type URL of a refusal detail.
const REFUSAL: &str = js_proto::refusal::REFUSAL_TYPE_URL;

/// A status of `code` and `message` whose details are `details`, encoded.
fn status_with(code: Code, message: &str, details: Vec<u8>) -> Status {
    Status::with_details(code, message, details.into())
}

/// The details holding `details`.
fn envelope(details: Vec<prost_types::Any>) -> Vec<u8> {
    RpcStatus {
        code: 9,
        message: "m".to_owned(),
        details,
    }
    .encode_to_vec()
}

/// Asserts how a status's details select its row through the router: a refusal decides over
/// the status's own code and message (a refusal number this version does not know is a 500
/// `internal`, a known refusal under a `NOT_FOUND` is still its own row); details that do
/// not decode, a detail of another type, and a refusal detail whose payload does not decode
/// leave the row to the code and the message. Every body is exact, so no detail text leaks.
/// Catches a refusal ignored, a corrupt detail answered as a refusal, and leaked details.
#[tokio::test]
async fn status_details_select_the_row_or_are_ignored() {
    let newer = js_proto::v1::Refusal {
        code: 99,
        detail: "a newer refusal".to_owned(),
    };
    let capable = js_proto::v1::Refusal {
        code: RefusalCode::NoCapableHost.into(),
        detail: "secret detail".to_owned(),
    };
    let rows = [
        (
            status_with(
                Code::FailedPrecondition,
                "not-finished",
                envelope(vec![detail(REFUSAL, newer.encode_to_vec())]),
            ),
            500,
            json!({ "error": "internal" }),
        ),
        (
            status_with(
                Code::NotFound,
                "unknown-job",
                envelope(vec![detail(REFUSAL, capable.encode_to_vec())]),
            ),
            422,
            json!({ "error": "no-capable-host" }),
        ),
        (
            status_with(Code::NotFound, "unknown-job", vec![0x00, 0x01]),
            404,
            json!({ "error": "unknown-job" }),
        ),
        (
            status_with(
                Code::NotFound,
                "expired",
                envelope(vec![detail(
                    "type.example.org/other.Detail",
                    vec![0x0a, 0x01, b'x'],
                )]),
            ),
            410,
            json!({ "error": "expired" }),
        ),
        (
            status_with(
                Code::Internal,
                "state-write-failed",
                envelope(vec![detail(REFUSAL, vec![0x12, 0x05, b'x'])]),
            ),
            500,
            json!({ "error": "state-write-failed" }),
        ),
    ];
    for (status, code, body) in rows {
        let shown = format!("{:?} {}", status.code(), status.message());
        let router = router(Arc::new(Stub::failing(status)));
        let reply = send(&router, "GET", "/v1/jobs/job-1", "").await;
        assert_eq!(reply.status.as_u16(), code, "{shown}");
        assert_eq!(reply.json(), body, "{shown}");
    }
}

/// A service whose `GetJob` for the job `first` waits for a signal the test sends, and
/// answers every job with its own id.
struct Gated {
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

#[tonic::async_trait]
impl JobService for Gated {
    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<tonic::Response<GetJobResponse>, Status> {
        let id = request.into_inner().job_id;
        let wait = match id.as_str() {
            "first" => self.release.lock().expect("not poisoned").take(),
            _ => None,
        };
        if let Some(wait) = wait {
            wait.await.expect("the test releases the first call");
        }
        let job = js_proto::v1::Job {
            id,
            ..js_proto::v1::Job::default()
        };
        Ok(tonic::Response::new(GetJobResponse { job: Some(job) }))
    }
}

/// Asserts that two calls in flight at once each get their own answer: the first call is
/// held inside the service until the second has been answered (an explicit signal, no
/// timing), and each answer names the job of its own request. Catches state shared between
/// requests of one router.
#[tokio::test]
async fn interleaved_calls_get_their_own_answers() {
    let (release, wait) = oneshot::channel();
    let router = router(Arc::new(Gated {
        release: Mutex::new(Some(wait)),
    }));
    let (first, second) = tokio::join!(send(&router, "GET", "/v1/jobs/first", ""), async {
        let second = send(&router, "GET", "/v1/jobs/second", "").await;
        release.send(()).expect("the first call waits");
        second
    });
    assert_eq!(first.json()["job"]["id"], json!("first"));
    assert_eq!(second.json()["job"]["id"], json!("second"));
}

/// Asserts that the fleet, usage and log calls answer over HTTP the message the trait
/// answers, field for field, for populated messages: hosts with every field, a host at its
/// defaults and one whose optional cordon time is present and zero, pools, providers with
/// an optional price of zero, usage records and an empty usage page, logs with bytes, a
/// cordon's answer, and a job in a state this version does not know (written as its
/// number). Catches a populated field lost or mapped wrongly, and optional presence lost.
#[tokio::test]
async fn fleet_usage_and_log_answers_survive_the_mapping() {
    let (stub, router) = serve();
    macro_rules! same {
        ($method:ident, $request:expr, $response:ty, $name:literal, $uri:literal) => {{
            let direct = stub
                .$method(Request::new($request))
                .await
                .expect("the stub answers")
                .into_inner();
            let reply = send(&router, "GET", $uri, "").await;
            let name = concat!("jobscheduler.v1.", $name);
            assert_eq!(
                from_json::<$response>(name, &reply.body),
                direct,
                "{}",
                $uri
            );
            reply
        }};
    }
    let hosts = same!(
        list_hosts,
        js_proto::v1::ListHostsRequest::default(),
        js_proto::v1::ListHostsResponse,
        "ListHostsResponse",
        "/v1/hosts"
    );
    assert_eq!(hosts.json()["hosts"][2]["cordonExpiresAtMs"], json!("0"));
    same!(
        list_pools,
        js_proto::v1::ListPoolsRequest::default(),
        js_proto::v1::ListPoolsResponse,
        "ListPoolsResponse",
        "/v1/pools"
    );
    same!(
        list_providers,
        js_proto::v1::ListProvidersRequest::default(),
        js_proto::v1::ListProvidersResponse,
        "ListProvidersResponse",
        "/v1/providers"
    );
    same!(
        get_usage,
        GetUsageRequest::default(),
        js_proto::v1::GetUsageResponse,
        "GetUsageResponse",
        "/v1/usage"
    );
    let empty = same!(
        get_usage,
        GetUsageRequest {
            page_token: "empty".to_owned(),
            ..GetUsageRequest::default()
        },
        js_proto::v1::GetUsageResponse,
        "GetUsageResponse",
        "/v1/usage?pageToken=empty"
    );
    assert_eq!(empty.body, b"{}");
    same!(
        get_logs,
        js_proto::v1::GetLogsRequest {
            job_id: "x-1".to_owned()
        },
        js_proto::v1::GetLogsResponse,
        "GetLogsResponse",
        "/v1/jobs/x-1/logs"
    );
    let listed = same!(
        list_jobs,
        ListJobsRequest::default(),
        ListJobsResponse,
        "ListJobsResponse",
        "/v1/jobs"
    );
    assert_eq!(listed.json()["jobs"][2]["state"], json!(42));
}

/// Asserts that a cordon's body sets its fields beside the host the path names, and that a
/// submit's body reaches the service as the job. Catches a body that is dropped.
#[tokio::test]
async fn a_body_sets_the_fields_of_a_change() {
    let (stub, router) = serve();
    let reply = send(
        &router,
        "POST",
        "/v1/hosts/host-a:cordon",
        r#"{"cordon_secs":60}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        stub.only_request::<CordonHostRequest>(),
        CordonHostRequest {
            host_id: "host-a".to_owned(),
            cordon_secs: Some(60),
        }
    );

    let (stub, router) = serve();
    let spec = common::spec();
    let descriptor = pool()
        .get_message_by_name("jobscheduler.v1.SubmitJobRequest")
        .expect("the request message is defined");
    let request = SubmitJobRequest {
        spec: Some(spec.clone()),
    };
    let message = DynamicMessage::decode(descriptor, request.encode_to_vec().as_slice())
        .expect("the request decodes");
    let body = serde_json::to_string(&message).expect("the request is written as JSON");
    let reply = send(&router, "POST", "/v1/jobs", body).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(stub.only_request::<SubmitJobRequest>(), request);
}
