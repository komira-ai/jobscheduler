//! The router over a stub service, request by request through tower `oneshot`: the routes,
//! the JSON mapping, the decoding of paths, queries and bodies, the limits and the answers
//! that are not a call's response. No socket is opened.

mod common;

use std::io;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use js_api_gateway::{BODY_LIMIT, ROUTES, Verb, router};
use js_proto::refusal::refusal_status;
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    CancelJobRequest, CordonHostRequest, GetJobRequest, GetJobResponse, GetResultRequest,
    GetResultResponse, GetUsageRequest, JobState, ListJobsRequest, ListJobsResponse, RefusalCode,
    SubmitJobRequest,
};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, Value};
use serde_json::json;
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
/// path. Catches a fallback route such as `POST /<service>/<method>`.
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
        for method in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
            let served = ROUTES
                .iter()
                .any(|other| other.verb.as_str() == method && fill(other.template) == path);
            // `GET /v1/jobs/x-1:cancel` is `GetJob` of the job id `x-1:cancel`, a 400 that
            // bad_job_id_is_a_400_naming_the_field asserts.
            let job_read = method == "GET" && path.starts_with("/v1/jobs/") && path.contains(':');
            if served || job_read {
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
    let direct = Stub::default();

    let grpc = direct
        .get_job(Request::new(GetJobRequest {
            job_id: "job-1".to_owned(),
        }))
        .await
        .expect("the stub answers")
        .into_inner();
    let reply = send(&router, "GET", "/v1/jobs/job-1", "").await;
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

    assert_eq!(stub.rpcs(), ["GetJob", "ListJobs", "GetResult"]);
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
        ("POST", "/v1/jobs/bad~id:cancel".to_owned(), "jobId"),
        ("GET", "/v1/jobs/job-1:cancel".to_owned(), "jobId"),
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
/// without a key, a key on a field that is not a map, a field and a map entry given twice,
/// a value of the wrong kind, text that is not UTF-8, a query on a route whose body is the
/// request, and a body on a route that takes none. Catches a malformed query that reaches
/// the service.
#[tokio::test]
async fn a_malformed_query_is_a_400() {
    let (stub, router) = serve();
    let twice = "the field is set twice";
    let malformed = "a map field is set as name[key]=value, any other field as name=value";
    let cases = [
        (
            "GET",
            "/v1/jobs?bogus=1",
            invalid("bogus", "the request has no such field"),
        ),
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
        (
            "GET",
            "/v1/jobs?nope[a]=1",
            invalid("nope[a]", "the request has no such field"),
        ),
        (
            "GET",
            "/v1/jobs?pageSize=1&page_size=2",
            invalid("page_size", twice),
        ),
        (
            "GET",
            "/v1/jobs?labels[a]=1&labels[a]=2",
            invalid("labels[a]", twice),
        ),
        (
            "GET",
            "/v1/jobs?pageSize=ten",
            invalid(
                "pageSize",
                "the field is unknown, or does not hold a value of its type",
            ),
        ),
        (
            "GET",
            "/v1/jobs?states=JOB_STATE_NONE",
            invalid(
                "states",
                "the field is unknown, or does not hold a value of its type",
            ),
        ),
        (
            "GET",
            "/v1/jobs?page_token=%FF",
            json!({
                "error": "invalid-argument",
                "reason": "the query string does not decode as UTF-8 text",
            }),
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

/// Asserts the 400s of a body and the field each names: not JSON, JSON that is not an
/// object, a value of the wrong kind (at the top, inside the job, in a list and in a map),
/// a message field that is not an object, the two names of one optional field inside the
/// job (the job is named) and at the top (no field is named), and a body that cannot be
/// read. Catches a malformed body that reaches the service, and a 400 that names the wrong
/// field.
#[tokio::test]
async fn a_malformed_body_is_a_400() {
    let (stub, router) = serve();
    let wrong = "the field is unknown, or does not hold a value of its type";
    let cases = [
        (
            "/v1/jobs",
            "{".to_owned(),
            json!({ "error": "invalid-argument", "reason": "the body is not valid JSON" }),
        ),
        (
            "/v1/jobs",
            "[]".to_owned(),
            json!({ "error": "invalid-argument", "reason": "the body is not a JSON object" }),
        ),
        (
            "/v1/jobs",
            json!({ "spec": 5 }).to_string(),
            invalid("spec", wrong),
        ),
        (
            "/v1/jobs",
            json!({ "spec": { "priority": "high" } }).to_string(),
            invalid("spec.priority", wrong),
        ),
        (
            "/v1/jobs",
            json!({ "spec": { "args": [1] } }).to_string(),
            invalid("spec.args", wrong),
        ),
        (
            "/v1/jobs",
            json!({ "spec": { "labels": { "a": 1 } } }).to_string(),
            invalid("spec.labels", wrong),
        ),
        (
            "/v1/jobs",
            json!({ "spec": { "wallLimitSeconds": "1", "wall_limit_seconds": "2" } }).to_string(),
            invalid("spec", wrong),
        ),
        (
            "/v1/hosts/host-a:cordon",
            json!({ "cordonSecs": 1, "cordon_secs": 2 }).to_string(),
            json!({ "error": "invalid-argument", "reason": wrong }),
        ),
    ];
    for (path, body, expected) in cases {
        let reply = send(&router, "POST", path, body.clone()).await;
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
