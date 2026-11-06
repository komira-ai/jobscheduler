//! The decoding of requests through the router over a stub service, request by request
//! through tower `oneshot`: path variables, queries, bodies, ids, integers, `null`, fields
//! the path sets, and the body limit. No socket is opened.

mod common;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use js_api_gateway::BODY_LIMIT;
use js_proto::v1::{
    CordonHostRequest, GetJobRequest, GetUsageRequest, JobSpec, JobState, ListJobsRequest,
    SubmitJobRequest,
};
use prost::Message;
use prost_reflect::DynamicMessage;
use serde_json::json;
use tonic::codegen::tokio_stream;

use common::{BIG, HEAD, TAIL, invalid, map, pool, send, serve, submit_body};

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

/// Asserts that a query sets the fields of `ListJobs` by JSON name or proto name: a
/// repeated enum by repeating it, a map entry as `labels[key]`, a 64-bit value as text, a
/// percent-encoded value and `+` as a space; and that `GetUsage` takes its filters so too.
/// Catches a query field that is dropped or mangled.
#[tokio::test]
async fn a_query_sets_the_fields_of_a_read() {
    let (stub, router) = serve();
    let query = "states=JOB_STATE_QUEUED&states=JOB_STATE_RUNNING&labels[team]=ml&\
                 labels%5Bkind%5D=a+b&queuedAfterMs=9007199254740993&queued_before_ms=7&\
                 pageSize=10&page_token=t%2F1&client=ci-1&&";
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
            client: Some("ci-1".to_owned()),
        }
    );
}

/// Asserts the 400s of a query and the field each names: an unknown parameter, a map
/// without a key, a key on a field that is not a map, a field and a map entry given twice
/// (also when two spellings decode to the same name or key), a value of the wrong kind,
/// malformed percent escapes (trailing, incomplete, not hexadecimal) in a name and in a
/// value, text that is not UTF-8, a query on a route whose body is the request, and a body
/// on a route that takes none. Every 400 names the field by its JSON name (a proto-name
/// spelling such as `page_size` is named `pageSize`), and a parameter of a map entry by the
/// map field alone (`labels[sensitive]` is `labels`), also when its value does not decode,
/// since a map key is a value of the request; a name that is no field is named as written
/// without its key. Catches a malformed query that reaches the service, a field named by
/// the spelling the client chose, and a map key echoed in an answer.
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
            invalid("pageSize", malformed),
        ),
        (
            "GET",
            "/v1/jobs?states[a]=JOB_STATE_QUEUED",
            invalid("states", malformed),
        ),
        ("GET", "/v1/jobs?nope[a]=1", invalid("nope", unknown)),
        (
            "GET",
            "/v1/jobs?pageSize=1&page_size=2",
            invalid("pageSize", twice),
        ),
        (
            "GET",
            "/v1/jobs?pageSize=1&page%53ize=2",
            invalid("pageSize", twice),
        ),
        (
            "GET",
            "/v1/jobs?labels[a]=1&labels[a]=2",
            invalid("labels", twice),
        ),
        (
            "GET",
            "/v1/jobs?labels[a]=1&labels%5Ba%5D=2",
            invalid("labels", twice),
        ),
        ("GET", "/v1/jobs?pageSize=ten", invalid("pageSize", wrong)),
        (
            "GET",
            "/v1/jobs?states=JOB_STATE_NONE",
            invalid("states[0]", wrong),
        ),
        ("GET", "/v1/jobs?page_token=%FF", invalid("pageToken", text)),
        ("GET", "/v1/jobs?pageToken=%zz", invalid("pageToken", text)),
        ("GET", "/v1/jobs?pageToken=%4", invalid("pageToken", text)),
        ("GET", "/v1/jobs?pageToken=a%", invalid("pageToken", text)),
        (
            "GET",
            "/v1/jobs?labels[sensitive]=%zz",
            invalid("labels", text),
        ),
        (
            "GET",
            "/v1/jobs?labels[sensitive]=%FF",
            invalid("labels", text),
        ),
        (
            "POST",
            "/v1/jobs?labels[sensitive]=1",
            invalid(
                "labels",
                "this route takes its fields in the body, not in the query",
            ),
        ),
        (
            "POST",
            "/v1/hosts/host-a:cordon?cordon_secs=1",
            invalid(
                "cordonSecs",
                "this route takes its fields in the body, not in the query",
            ),
        ),
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
        ("/v1/jobs", r#"{"spec":[]}"#, invalid("spec", wrong)),
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
