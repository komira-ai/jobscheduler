//! Property tests and a table of malformed inputs for the request decoder
//! ([`crate::input::build`]): a request encoded as proto3 JSON (or as a query string)
//! decodes to the request it was made from, whatever bytes or JSON the decoder is given it
//! answers instead of panicking, and each malformed input has its one exact answer.
//!
//! Every property runs with a fixed seed and no failure file, so a run is the same on every
//! machine.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    Accelerator, CancelJobRequest, Constraints, GetJobRequest, JobFile, JobSpec, ListJobsRequest,
    Needs, PortForward, SubmitJobRequest, SubmitJobResponse,
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use proptest::collection::{btree_map, vec};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use serde_json::{Value as Json, json};
use tonic::Status;
use tower::ServiceExt;

use crate::answer::Answer;
use crate::input::{
    BAD_ID, MEMBER_TWICE, NO_BODY, NO_QUERY, NOT_AN_OBJECT, NOT_JSON, NOT_TEXT, SET_BY_PATH, TWICE,
    UNKNOWN_FIELD, WRONG_VALUE, build,
};
use crate::{BODY_LIMIT, ROUTES};

/// The configuration of every property: a fixed seed, no failure file.
fn config(cases: u32, seed: u64) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(seed),
        ..ProptestConfig::default()
    }
}

/// The pool of the job service, decoded once.
fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
    })
}

/// The descriptor of the message `name` of the job service.
fn descriptor(name: &str) -> MessageDescriptor {
    pool()
        .get_message_by_name(&format!("jobscheduler.v1.{name}"))
        .expect("the message is defined")
}

/// The proto3 JSON of `request`, as a client would write the body.
fn encode<M: Message>(descriptor: &MessageDescriptor, request: &M) -> Vec<u8> {
    let message = DynamicMessage::decode(descriptor.clone(), request.encode_to_vec().as_slice())
        .expect("a generated message decodes");
    serde_json::to_vec(&message).expect("a message serializes")
}

/// The request `build` makes from `body` as `M`.
fn decoded<M: Message + Default>(input: &MessageDescriptor, body: &[u8]) -> M {
    let message = build(input, true, &[], None, body).expect("the body decodes");
    message.transcode_to::<M>().expect("the message converts")
}

/// The answer `build` gives to `body` for the message `name`, a route that takes a body.
fn refused(name: &str, body: &[u8]) -> Answer {
    build(&descriptor(name), true, &[], None, body).expect_err("the body is refused")
}

/// A short text of any characters.
fn text() -> impl Strategy<Value = String> {
    "\\PC{0,8}"
}

/// A job id in its grammar, or none (the service mints one).
fn job_id() -> impl Strategy<Value = String> {
    "[A-Za-z0-9._-]{0,63}"
}

/// A set of up to four entries of any text.
fn entries() -> impl Strategy<Value = BTreeMap<String, String>> {
    btree_map(text(), text(), 0..4)
}

/// A value of the enum with the numbers `0..count`.
fn variant(count: i32) -> impl Strategy<Value = i32> {
    0..count
}

fn accelerator() -> impl Strategy<Value = Accelerator> {
    (
        proptest::option::of(text()),
        proptest::option::of(text()),
        proptest::option::of(any::<u32>()),
        proptest::option::of(any::<u32>()),
        proptest::option::of(text()),
        proptest::option::of(text()),
    )
        .prop_map(
            |(vendor, model, count, memory_per_device_gib, interconnect, form_factor)| {
                Accelerator {
                    vendor,
                    model,
                    count,
                    memory_per_device_gib,
                    interconnect,
                    form_factor,
                }
            },
        )
}

fn needs() -> impl Strategy<Value = Needs> {
    (
        any::<u64>(),
        any::<u64>(),
        proptest::option::of(any::<u64>()),
        any::<u32>(),
        text(),
        any::<bool>(),
    )
        .prop_map(
            |(cpu_millis, memory_bytes, disk_bytes, gpus, gpu_class, whole_host)| Needs {
                cpu_millis,
                memory_bytes,
                disk_bytes,
                gpus,
                gpu_class,
                whole_host,
            },
        )
}

fn constraints() -> impl Strategy<Value = Constraints> {
    (
        text(),
        text(),
        variant(3),
        text(),
        proptest::option::of(accelerator()),
    )
        .prop_map(|(os, arch, exec, isa_level, accelerator)| Constraints {
            os,
            arch,
            exec,
            isa_level,
            accelerator,
        })
}

fn job_spec() -> impl Strategy<Value = JobSpec> {
    let strings = (job_id(), text(), text(), vec(text(), 0..4), entries());
    let resources = (
        proptest::option::of(needs()),
        proptest::option::of(constraints()),
        proptest::option::of(any::<u64>()),
        variant(4),
    );
    let rest = (
        vec((any::<u32>(), any::<u32>()), 0..4),
        vec((text(), text()), 0..3),
        any::<i32>(),
        entries(),
    );
    (strings, resources, rest).prop_map(
        |(
            (job_id, image, command, args, env),
            (needs, constraints, wall_limit_seconds, network),
            (ports, files, priority, labels),
        )| JobSpec {
            job_id,
            image,
            command,
            args,
            env: env.into_iter().collect(),
            needs,
            constraints,
            wall_limit_seconds,
            network,
            ports: ports
                .into_iter()
                .map(|(host_port, container_port)| PortForward {
                    host_port,
                    container_port,
                })
                .collect(),
            files: files
                .into_iter()
                .map(|(name, contents)| JobFile { name, contents })
                .collect(),
            priority,
            labels: labels.into_iter().collect(),
        },
    )
}

fn list_jobs() -> impl Strategy<Value = ListJobsRequest> {
    (
        vec(variant(10), 0..4),
        entries(),
        (
            proptest::option::of(any::<u64>()),
            proptest::option::of(any::<u64>()),
            proptest::option::of(text()),
        ),
        any::<u32>(),
        text(),
    )
        .prop_map(
            |(
                states,
                labels,
                (queued_after_ms, queued_before_ms, client),
                page_size,
                page_token,
            )| {
                ListJobsRequest {
                    states,
                    labels: labels.into_iter().collect(),
                    queued_after_ms,
                    queued_before_ms,
                    client,
                    page_size,
                    page_token,
                }
            },
        )
}

/// `text` as a query string writes it: every byte but a letter or digit escaped.
fn escaped(text: &str) -> String {
    utf8_percent_encode(text, NON_ALPHANUMERIC).to_string()
}

/// The query string that sets the fields of `request`.
fn query_of(request: &ListJobsRequest) -> String {
    let mut pairs = Vec::new();
    for state in &request.states {
        let name = js_proto::v1::JobState::try_from(*state).expect("a known state");
        pairs.push(format!("states={}", name.as_str_name()));
    }
    for (key, value) in &request.labels {
        pairs.push(format!("labels[{}]={}", escaped(key), escaped(value)));
    }
    if let Some(ms) = request.queued_after_ms {
        pairs.push(format!("queuedAfterMs={ms}"));
    }
    if let Some(ms) = request.queued_before_ms {
        pairs.push(format!("queued_before_ms={ms}"));
    }
    if let Some(client) = &request.client {
        pairs.push(format!("client={}", escaped(client)));
    }
    pairs.push(format!("pageSize={}", request.page_size));
    pairs.push(format!("page_token={}", escaped(&request.page_token)));
    pairs.join("&")
}

/// A field name or a value a request body might hold, to reach past the first level.
fn word() -> impl Strategy<Value = String> {
    prop_oneof![
        proptest::sample::select(vec![
            "spec",
            "jobId",
            "job_id",
            "image",
            "args",
            "env",
            "labels",
            "needs",
            "cpuMillis",
            "wallLimitSeconds",
            "wall_limit_seconds",
            "priority",
            "ports",
            "hostPort",
            "files",
            "constraints",
            "accelerator",
            "count",
            "network",
            "states",
            "pageSize",
            "hostId",
            "NETWORK_LEVEL_OPEN",
            "JOB_STATE_QUEUED",
            "9007199254740993",
            "-1",
            "bad~id",
            "",
        ])
        .prop_map(str::to_owned),
        text(),
    ]
}

/// Any JSON value built from [`word`]s, numbers of every kind and nesting.
fn json_value() -> impl Strategy<Value = Json> {
    let leaf = prop_oneof![
        Just(Json::Null),
        any::<bool>().prop_map(Json::from),
        any::<i64>().prop_map(Json::from),
        any::<u64>().prop_map(Json::from),
        (-1.0e30f64..1.0e30).prop_map(Json::from),
        word().prop_map(Json::from),
    ];
    leaf.prop_recursive(5, 48, 4, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..4).prop_map(Json::Array),
            vec((word(), inner), 0..4)
                .prop_map(|members| Json::Object(members.into_iter().collect())),
        ]
    })
}

/// Asserts that `answer` is a 400 `invalid-argument` whose body gives a reason.
fn assert_refusal(answer: &Answer) {
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
    assert_eq!(answer.code(), "invalid-argument");
    assert_eq!(answer.body().get("reason").map(Json::is_string), Some(true));
}

/// Runs `build` on every route's request message with `body` and `query`, and checks what
/// it gives back: a message that encodes to JSON and decodes again to itself (when the
/// route takes a body), or a refusal.
fn every_route(body: &[u8], query: Option<&str>) {
    for route in ROUTES {
        let input = pool().get_message_by_name(route.input).expect("defined");
        match build(&input, route.body, &[], query, body) {
            Ok(message) if route.body => {
                let again = serde_json::to_vec(&message).expect("serializes");
                assert_eq!(build(&input, true, &[], None, &again), Ok(message));
            }
            Ok(_) => {}
            Err(answer) => assert_refusal(&answer),
        }
    }
}

proptest! {
    #![proptest_config(config(256, 0x6a6f_6273_0001))]

    /// A submit request, written as proto3 JSON, decodes to the request it was made from.
    #[test]
    fn a_submit_request_decodes_from_its_json(spec in job_spec()) {
        let request = SubmitJobRequest { spec: Some(spec) };
        let input = descriptor("SubmitJobRequest");
        let body = encode(&input, &request);
        prop_assert_eq!(decoded::<SubmitJobRequest>(&input, &body), request);
    }

    /// A list request, written as a query string, decodes to the request it was made from.
    #[test]
    fn a_list_request_decodes_from_its_query(request in list_jobs()) {
        let input = descriptor("ListJobsRequest");
        let query = query_of(&request);
        let message = build(&input, false, &[], Some(&query), b"").expect("the query decodes");
        prop_assert_eq!(message.transcode_to::<ListJobsRequest>().expect("converts"), request);
    }

    /// A list request, written as a JSON body to a route that takes a body, decodes to the
    /// request it was made from.
    #[test]
    fn a_list_request_decodes_from_its_json(request in list_jobs()) {
        let input = descriptor("ListJobsRequest");
        let body = encode(&input, &request);
        prop_assert_eq!(decoded::<ListJobsRequest>(&input, &body), request);
    }

    /// A job id in the path sets the request's id, for a route with a body and one without.
    #[test]
    fn a_path_job_id_sets_the_request(id in "[A-Za-z0-9._-]{1,63}") {
        let variables = [("job_id".to_owned(), id.clone())];
        let cancel = build(&descriptor("CancelJobRequest"), true, &variables, None, b"")
            .expect("decodes");
        prop_assert_eq!(
            cancel.transcode_to::<CancelJobRequest>().expect("converts"),
            CancelJobRequest { job_id: id.clone() }
        );
        let get = build(&descriptor("GetJobRequest"), false, &variables, None, b"")
            .expect("decodes");
        prop_assert_eq!(
            get.transcode_to::<GetJobRequest>().expect("converts"),
            GetJobRequest { job_id: id }
        );
    }

    /// Any bytes as a body, and any text as a query, are answered with a request or a 400,
    /// never a panic; a request that is made encodes to a body that decodes to it.
    #[test]
    fn arbitrary_bytes_do_not_panic(
        body in vec(any::<u8>(), 0..256),
        query in "[ -~%&=\\[\\]+]{0,40}",
    ) {
        every_route(&body, None);
        every_route(b"", Some(&query));
    }

    /// A valid body with one byte changed or the end cut off is answered with a request or a
    /// 400, never a panic.
    #[test]
    fn a_damaged_body_does_not_panic(
        spec in job_spec(),
        at in any::<proptest::sample::Index>(),
        byte in any::<u8>(),
        cut in any::<bool>(),
    ) {
        let mut body = encode(&descriptor("SubmitJobRequest"), &SubmitJobRequest { spec: Some(spec) });
        let at = at.index(body.len());
        if cut {
            body.truncate(at);
        } else {
            body[at] = byte;
        }
        every_route(&body, None);
    }

    /// Any JSON value, as a body, is answered with a request or a 400, never a panic.
    #[test]
    fn arbitrary_json_does_not_panic(value in json_value()) {
        let body = serde_json::to_vec(&value).expect("serializes");
        every_route(&body, None);
    }

    /// Any text as a path variable is answered with a request or a 400, never a panic.
    #[test]
    fn arbitrary_path_text_does_not_panic(text in "[ -~]{0,70}", field in 0..2usize) {
        let name = ["job_id", "host_id"][field];
        let input = descriptor(["GetJobRequest", "CordonHostRequest"][field]);
        let variables = [(name.to_owned(), text)];
        if let Err(answer) = build(&input, false, &variables, None, b"") {
            assert_refusal(&answer);
        }
    }
}

/// The 400 `invalid-argument` of `reason` naming `field`.
fn invalid(field: Option<&str>, reason: &'static str) -> Answer {
    Answer::invalid(field.map(str::to_owned), reason)
}

/// Asserts that each malformed body of a submit is refused with its one answer: the status,
/// the code, the field and the reason. Truncated JSON, bytes after the object, a text that
/// is not UTF-8, a value that is not an object, a member given twice, a field of the wrong
/// name, a value of the wrong kind or of an enum that has no such name, a number a JSON
/// reader would round, a field given under both its names and an id outside its grammar.
/// Catches a malformed body that reaches the service, or that is answered with another
/// status, field or reason.
#[test]
fn a_malformed_body_has_its_one_answer() {
    let cases: &[(&str, &[u8], Option<&str>, &str)] = &[
        ("truncated", br#"{"spec":{"image":"a""#, None, NOT_JSON),
        ("only an opening brace", b"{", None, NOT_JSON),
        (
            "not utf-8",
            b"{\"spec\":{\"image\":\"\xff\"}}",
            None,
            NOT_JSON,
        ),
        ("bytes after the object", b"{} x", None, NOT_JSON),
        ("a second object", b"{}{}", None, NOT_JSON),
        ("not JSON", b"spec", None, NOT_JSON),
        ("a trailing comma", br#"{"spec":{},}"#, None, NOT_JSON),
        ("an array", b"[]", None, NOT_AN_OBJECT),
        ("a number", b"1", None, NOT_AN_OBJECT),
        ("null", b"null", None, NOT_AN_OBJECT),
        (
            "a member twice",
            br#"{"spec":{},"spec":{}}"#,
            None,
            MEMBER_TWICE,
        ),
        (
            "a member twice inside",
            br#"{"spec":{"image":"a","image":"b"}}"#,
            None,
            MEMBER_TWICE,
        ),
        (
            "an unknown field",
            br#"{"nope":1}"#,
            Some("nope"),
            WRONG_VALUE,
        ),
        (
            "an unknown field inside",
            br#"{"spec":{"imag":"x"}}"#,
            Some("spec.imag"),
            WRONG_VALUE,
        ),
        (
            "an enum without that name",
            br#"{"spec":{"network":"NETWORK_LEVEL_WIFI"}}"#,
            Some("spec.network"),
            WRONG_VALUE,
        ),
        (
            "a string for an object",
            br#"{"spec":"x"}"#,
            Some("spec"),
            WRONG_VALUE,
        ),
        (
            "a number for a string",
            br#"{"spec":{"image":5}}"#,
            Some("spec.image"),
            WRONG_VALUE,
        ),
        (
            "a fraction for an integer",
            br#"{"spec":{"priority":1.5}}"#,
            Some("spec.priority"),
            WRONG_VALUE,
        ),
        (
            "an exponent for an integer",
            br#"{"spec":{"wallLimitSeconds":1e3}}"#,
            Some("spec.wallLimitSeconds"),
            WRONG_VALUE,
        ),
        (
            "a number past 64 bits",
            br#"{"spec":{"wallLimitSeconds":18446744073709551616}}"#,
            Some("spec.wallLimitSeconds"),
            WRONG_VALUE,
        ),
        (
            "a number past 32 bits",
            br#"{"spec":{"priority":2147483648}}"#,
            Some("spec.priority"),
            WRONG_VALUE,
        ),
        (
            "an element of a list",
            br#"{"spec":{"ports":[{"hostPort":1},{"hostPort":1.5}]}}"#,
            Some("spec.ports[1].hostPort"),
            WRONG_VALUE,
        ),
        (
            "both names of a field",
            br#"{"spec":{"jobId":"a","job_id":"b"}}"#,
            Some("spec.jobId"),
            TWICE,
        ),
        (
            "an id with a character outside the grammar",
            br#"{"spec":{"jobId":"bad~id"}}"#,
            Some("spec.jobId"),
            BAD_ID,
        ),
        (
            "an id of 64 bytes",
            format!(r#"{{"spec":{{"jobId":"{}"}}}}"#, "a".repeat(64))
                .leak()
                .as_bytes(),
            Some("spec.jobId"),
            BAD_ID,
        ),
    ];
    for (name, body, field, reason) in cases {
        assert_eq!(
            refused("SubmitJobRequest", body),
            invalid(*field, reason),
            "{name}"
        );
    }
}

/// Asserts that a number past 2^53 is read exactly when it is an integer in range, as a JSON
/// number and as a decimal string, so no rounding changes the request.
#[test]
fn an_integer_past_2_pow_53_is_read_exactly() {
    let input = descriptor("SubmitJobRequest");
    for body in [
        br#"{"spec":{"wallLimitSeconds":9007199254740993}}"#.as_slice(),
        br#"{"spec":{"wallLimitSeconds":"9007199254740993"}}"#.as_slice(),
    ] {
        let request: SubmitJobRequest = decoded(&input, body);
        let spec = request.spec.expect("the spec is set");
        assert_eq!(spec.wall_limit_seconds, Some((1 << 53) + 1));
    }
}

/// Asserts the answers of a route without a body and of one with: a body where none is
/// taken, a query where none is taken, a field the path sets, an unknown query field, a
/// field set twice, a malformed escape and a map key written the wrong way.
#[test]
fn a_malformed_query_has_its_one_answer() {
    let list = descriptor("ListJobsRequest");
    let get = descriptor("GetJobRequest");
    let job = [("job_id".to_owned(), "j".to_owned())];
    let cases: Vec<(&str, Result<DynamicMessage, Answer>, Answer)> = vec![
        (
            "a body where none is taken",
            build(&list, false, &[], None, b"{}"),
            invalid(None, NO_BODY),
        ),
        (
            "a query where none is taken",
            build(&list, true, &[], Some("pageSize=1"), b""),
            invalid(Some("pageSize"), NO_QUERY),
        ),
        (
            "a field the path sets",
            build(&get, false, &job, Some("job_id=k"), b""),
            invalid(Some("jobId"), SET_BY_PATH),
        ),
        (
            "an unknown field",
            build(&list, false, &[], Some("nope=1"), b""),
            invalid(Some("nope"), UNKNOWN_FIELD),
        ),
        (
            "a field twice",
            build(&list, false, &[], Some("pageSize=1&page_size=2"), b""),
            invalid(Some("pageSize"), TWICE),
        ),
        (
            "a malformed escape in a value",
            build(&list, false, &[], Some("client=%zz"), b""),
            invalid(Some("client"), NOT_TEXT),
        ),
        (
            "a malformed escape in a name",
            build(&list, false, &[], Some("%zz=1"), b""),
            invalid(None, NOT_TEXT),
        ),
        (
            "a value that is not UTF-8",
            build(&list, false, &[], Some("client=%ff"), b""),
            invalid(Some("client"), NOT_TEXT),
        ),
        (
            "an id outside the grammar in the path",
            build(
                &get,
                false,
                &[("job_id".to_owned(), "a b".to_owned())],
                None,
                b"",
            ),
            invalid(Some("jobId"), BAD_ID),
        ),
    ];
    for (name, got, want) in cases {
        assert_eq!(got.expect_err("refused"), want, "{name}");
    }
}

/// A [`JobService`] that counts the submits it receives and answers each.
#[derive(Default)]
struct Counting(AtomicUsize);

#[tonic::async_trait]
impl JobService for Counting {
    async fn submit_job(
        &self,
        _: tonic::Request<SubmitJobRequest>,
    ) -> Result<tonic::Response<SubmitJobResponse>, Status> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(tonic::Response::new(SubmitJobResponse::default()))
    }
}

/// The status and JSON body the router answers to a submit with `body`, and the number of
/// submits the service received.
async fn submit(body: Vec<u8>) -> (StatusCode, Json, usize) {
    let service = Arc::new(Counting::default());
    let request = Request::builder()
        .method("POST")
        .uri("/v1/jobs")
        .body(Body::from(body))
        .expect("the request is well formed");
    let response = crate::router(service.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read");
    let json = serde_json::from_slice(&bytes).expect("the answer is JSON");
    (status, json, service.0.load(Ordering::Relaxed))
}

/// Asserts that a body of [`BODY_LIMIT`] bytes is decoded and called, and one of a byte more
/// is a 413 `request-too-large` that calls nothing, whatever it holds (valid JSON padded
/// with spaces, or bytes that are not JSON).
#[tokio::test]
async fn a_body_over_the_limit_is_413_and_one_at_it_is_decoded() {
    let mut at = br#"{"spec":{"image":"a"}}"#.to_vec();
    at.resize(BODY_LIMIT, b' ');
    assert_eq!(
        submit(at.clone()).await,
        (StatusCode::CREATED, json!({}), 1)
    );

    let mut over = at;
    over.push(b' ');
    let too_large = json!({ "error": "request-too-large" });
    assert_eq!(
        submit(over).await,
        (StatusCode::PAYLOAD_TOO_LARGE, too_large.clone(), 0)
    );
    assert_eq!(
        submit(vec![0xff; BODY_LIMIT + 1]).await,
        (StatusCode::PAYLOAD_TOO_LARGE, too_large, 0)
    );
}
