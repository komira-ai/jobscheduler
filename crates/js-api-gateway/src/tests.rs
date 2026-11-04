//! Tests of the crate's private paths: messages that do not convert between a descriptor and
//! a generated type, a router over an in-crate service (whose instantiation of the router's
//! generic code runs here, apart from the integration tests' stub), a router made from a
//! descriptor set that lacks the routes' messages, and the query values only a test
//! descriptor has (bools).

use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{GetJobRequest, GetLogsResponse, JobState, ListHostsRequest, ListHostsResponse};
use prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MapKey, Value};
use tonic::Status;
use tower::ServiceExt;

use crate::Verb;
use crate::answer::Answer;

/// The pool of the job service.
fn service_pool() -> DescriptorPool {
    DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// Asserts that a request message that does not convert to the method's request type, and
/// a response whose bytes do not decode as the route's response message, are each a 500
/// `internal` (a varint where a string is); that a response that decodes is answered as
/// the message; and that a status is answered by the error table. Catches a conversion
/// fault answered as anything but an internal error.
#[test]
fn a_message_that_does_not_convert_is_internal() {
    let pool = service_pool();
    let logs = pool
        .get_message_by_name("jobscheduler.v1.GetLogsResponse")
        .expect("defined");
    let job_request = pool
        .get_message_by_name("jobscheduler.v1.GetJobRequest")
        .expect("defined");
    let mut running = DynamicMessage::new(logs.clone());
    running.set_field_by_name("state", Value::EnumNumber(JobState::Running.into()));
    assert_eq!(
        crate::decode_as::<GetJobRequest>(&running),
        Err(Answer::internal())
    );

    let running_bytes = GetLogsResponse {
        state: JobState::Running.into(),
        ..GetLogsResponse::default()
    }
    .encode_to_vec();
    assert_eq!(
        crate::answer_of(&job_request, Ok(running_bytes.clone())),
        Err(Answer::internal())
    );
    assert_eq!(crate::answer_of(&logs, Ok(running_bytes)), Ok(running));
    assert_eq!(
        crate::answer_of(&logs, Err(Status::not_found("unknown-job"))),
        Err(Answer::of_status(&Status::not_found("unknown-job")))
    );
}

/// A service that answers `ListHosts` and nothing else: every other call is the generated
/// `UNIMPLEMENTED`.
struct Hosts;

#[tonic::async_trait]
impl JobService for Hosts {
    async fn list_hosts(
        &self,
        _: tonic::Request<ListHostsRequest>,
    ) -> Result<tonic::Response<ListHostsResponse>, Status> {
        Ok(tonic::Response::new(ListHostsResponse::default()))
    }
}

/// The status and body the router over `descriptors` answers to `method uri` with `body`.
async fn send(
    descriptors: &[u8],
    method: &str,
    uri: &str,
    body: &'static str,
) -> (StatusCode, Vec<u8>) {
    let router = crate::serve::router_over(Arc::new(Hosts), descriptors);
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body))
        .expect("the request is well formed");
    let response = router.oneshot(request).await.expect("the router answers");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read");
    (status, body.to_vec())
}

/// `template` with every `{variable}` replaced by `x-1`.
fn fill(template: &str) -> String {
    let mut path = String::new();
    for (index, part) in template.split(['{', '}']).enumerate() {
        path.push_str(if index % 2 == 1 { "x-1" } else { part });
    }
    path
}

/// Asserts that a router made from a descriptor set that does not decode, and from one that
/// decodes but lacks the routes' messages, answers 500 `internal`; and that with the
/// service's descriptors every route reaches its method of this service (`ListHosts`
/// answers 200 `{}`, every other one the generated 501 `unimplemented`), a bad id is a 400
/// and a path no route has a 404. Catches a router that panics or serves without its
/// messages, and a route of this instantiation of the generated `call` that reaches the
/// wrong method.
#[tokio::test]
async fn a_router_answers_from_its_messages_only() {
    for descriptors in [&b"\xff"[..], &[][..]] {
        let (status, body) = send(descriptors, "GET", "/v1/jobs/job-1", "").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, br#"{"error":"internal"}"#);
    }
    let set = js_proto::FILE_DESCRIPTOR_SET;
    for route in crate::ROUTES {
        let body = if route.body { "{}" } else { "" };
        let answer = send(set, route.verb.as_str(), &fill(route.template), body).await;
        let expected = match route.rpc {
            crate::Rpc::ListHosts => (StatusCode::OK, b"{}".to_vec()),
            _ => (
                StatusCode::NOT_IMPLEMENTED,
                br#"{"error":"unimplemented"}"#.to_vec(),
            ),
        };
        assert_eq!(answer, expected, "{}", route.template);
    }
    let (status, _) = send(set, "GET", "/v1/jobs/bad~id", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(set, "GET", "/v1/jobs/job-1/nothing", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A route of `rpc` at `template`, for the matching tests.
fn binding(route: &'static crate::Route) -> crate::serve::Binding {
    let pool = service_pool();
    let message = pool
        .get_message_by_name("jobscheduler.v1.GetJobRequest")
        .expect("defined");
    crate::serve::Binding {
        route,
        template: crate::annotation::Template::parse(route.template).expect("parses"),
        status: StatusCode::OK,
        input: message.clone(),
        output: message,
    }
}

/// A test route of `GetJob` at `/v1/things/{job_id}`.
static BY_ID: crate::Route = crate::Route {
    verb: Verb::Get,
    template: "/v1/things/{job_id}",
    rpc: crate::Rpc::GetJob,
    body: false,
    status: 200,
    input: "jobscheduler.v1.GetJobRequest",
    output: "jobscheduler.v1.GetJobRequest",
};

/// A test route of `ListJobs` at the literal `/v1/things/list`.
static LISTED: crate::Route = crate::Route {
    template: "/v1/things/list",
    rpc: crate::Rpc::ListJobs,
    ..BY_ID
};

/// A test route of `CancelJob` at `/v1/things/{job_id}:cancel`.
static CANCEL: crate::Route = crate::Route {
    verb: Verb::Post,
    template: "/v1/things/{job_id}:cancel",
    rpc: crate::Rpc::CancelJob,
    ..BY_ID
};

/// The method `find` picks for `method path` over `routes` in that order, or the answer
/// it refuses with.
fn pick(routes: &[&'static crate::Route], method: &str, path: &str) -> Result<crate::Rpc, Answer> {
    let bindings: Vec<crate::serve::Binding> = routes.iter().map(|route| binding(route)).collect();
    let method = Method::from_bytes(method.as_bytes()).expect("a method");
    crate::serve::find(&bindings, &method, path).map(|(binding, _)| binding.route.rpc)
}

/// Asserts that the most specific template that matches a path decides, in either order of
/// the routes: a literal segment before a variable (`/v1/things/list` is not the job id
/// `list`), and a custom verb before none (`GET /v1/things/x:cancel` is a 405 of the cancel
/// route allowing POST, not a read of the job id `x:cancel`); that HEAD, OPTIONS, TRACE
/// and CONNECT on a route's path are 405s allowing its method; and that a path no template
/// matches is a 404. Catches a first-match router that lets an earlier variable route shadow
/// a later, more specific one.
#[test]
fn the_most_specific_template_decides() {
    for order in [[&BY_ID, &LISTED], [&LISTED, &BY_ID]] {
        assert_eq!(
            pick(&order, "GET", "/v1/things/list"),
            Ok(crate::Rpc::ListJobs)
        );
        assert_eq!(pick(&order, "GET", "/v1/things/x"), Ok(crate::Rpc::GetJob));
    }
    for order in [[&BY_ID, &CANCEL], [&CANCEL, &BY_ID]] {
        assert_eq!(
            pick(&order, "POST", "/v1/things/x:cancel"),
            Ok(crate::Rpc::CancelJob)
        );
        assert_eq!(
            pick(&order, "GET", "/v1/things/x:cancel"),
            Err(Answer::method_not_allowed(vec![Verb::Post]))
        );
        for method in ["HEAD", "OPTIONS", "TRACE", "CONNECT"] {
            assert_eq!(
                pick(&order, method, "/v1/things/x"),
                Err(Answer::method_not_allowed(vec![Verb::Get])),
                "{method}"
            );
        }
    }
    assert_eq!(
        pick(&[&BY_ID], "GET", "/v1/elsewhere"),
        Err(Answer::not_found())
    );
}

/// Asserts that a response that does not serialize (an `Any` whose type is not in the pool)
/// is a 500 whose whole body is `{"error":"internal"}`, not the success status with the
/// serializer's text. Catches a serialization failure that bypasses the error table.
#[tokio::test]
async fn a_response_that_does_not_serialize_is_internal() {
    let pool = good_pool();
    let with_any = pool
        .get_message_by_name("gatewaytest.WithAny")
        .expect("defined");
    let any = pool
        .get_message_by_name("google.protobuf.Any")
        .expect("defined");
    let mut packed = DynamicMessage::new(any);
    packed.set_field_by_name(
        "type_url",
        Value::String("type.example.org/missing.Message".to_owned()),
    );
    let mut message = DynamicMessage::new(with_any);
    message.set_field_by_name("any", Value::Message(packed));
    let answer = crate::serve::respond(StatusCode::CREATED, &message).expect_err("refused");
    assert_eq!(answer, Answer::internal());
    let response = axum::response::IntoResponse::into_response(answer);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read");
    assert_eq!(&body[..], br#"{"error":"internal"}"#);
}

/// Asserts what the strict JSON reader expects, as serde words it in an error.
#[test]
fn the_strict_reader_expects_a_json_value() {
    let expected: &dyn serde::de::Expected = &crate::input::StrictVisitor;
    assert_eq!(expected.to_string(), "a JSON value");
}

/// Asserts that a query sets any call's optional, bytes and 64-bit fields by the proto3 JSON
/// mapping: base64 bytes (with `+`, `/` and `=` percent-encoded, since a bare `+` is a
/// space), the largest `u64` and the smallest `i64` as decimal strings, and an optional
/// field given as zero present; and that an absent optional field stays absent. Catches a
/// query mapping that only handles the job calls' current fields.
#[test]
fn a_query_sets_bytes_and_64_bit_values() {
    let request = good_pool()
        .get_message_by_name("gatewaytest.Req")
        .expect("defined");
    let query = "blob=%2B%2F8%3D&big=18446744073709551615&small=-9223372036854775808";
    let message = crate::input::build(&request, false, &[], Some(query), b"").expect("decodes");
    let field = |name: &str| {
        message
            .get_field_by_name(name)
            .map(|value| value.into_owned())
    };
    assert_eq!(field("blob"), Some(Value::Bytes(vec![0xfb, 0xff].into())));
    assert_eq!(field("big"), Some(Value::U64(u64::MAX)));
    assert_eq!(field("small"), Some(Value::I64(i64::MIN)));
    let zero = crate::input::build(&request, false, &[], Some("big=0"), b"").expect("decodes");
    assert!(zero.has_field_by_name("big"));
    assert!(!zero.has_field_by_name("small"));
}

/// Asserts that two fields of one oneof, each valid alone, are a 400 that names no field
/// (no single field is to blame), and that two names of one field are a 400 naming it.
#[test]
fn a_oneof_conflict_names_no_field() {
    let request = good_pool()
        .get_message_by_name("gatewaytest.Req")
        .expect("defined");
    let refused = crate::input::build(&request, true, &[], None, br#"{"left":"a","right":"b"}"#);
    assert_eq!(
        refused.map(|_| ()),
        Err(Answer::invalid(
            None,
            "the field is unknown, or does not hold a value of its type"
        ))
    );
    let refused = crate::input::build(
        &request,
        true,
        &[],
        None,
        br#"{"otherId":"a","other_id":"b"}"#,
    );
    assert_eq!(
        refused.map(|_| ()),
        Err(Answer::invalid(
            Some("otherId".to_owned()),
            "the field is set twice"
        ))
    );
}

/// Asserts that `src/gateway.rs` is exactly what `src/codegen.rs` writes for the job
/// service's options, and that the check fails for a route list that lost a route. Catches
/// a checked-in file edited by hand or left behind by a change of the options.
#[test]
fn generated_source_is_current() {
    let service = "jobscheduler.v1.JobService";
    let mut routes = crate::annotation::routes(&service_pool(), service).expect("the options pass");
    let generated = crate::codegen::render(service, &routes);
    assert_eq!(generated, include_str!("gateway.rs"));
    routes.pop();
    assert_ne!(
        crate::codegen::render(service, &routes),
        include_str!("gateway.rs")
    );
}

/// Asserts the Rust names the generator gives methods and messages, as prost and tonic
/// write them, acronyms included, and that a service of acronyms renders with them.
/// Catches a name the generated trait or message type does not have.
#[test]
fn generated_names_follow_prost() {
    assert_eq!(crate::codegen::method_name("GetJob"), "get_job");
    assert_eq!(
        crate::codegen::method_name("GetHTTPUsage"),
        "get_http_usage"
    );
    assert_eq!(
        crate::codegen::message_type("jobscheduler.v1.GetJobRequest"),
        "::js_proto::v1::GetJobRequest"
    );
    assert_eq!(
        crate::codegen::message_type("gatewaytest.HTTPRequest"),
        "::js_proto::v1::HttpRequest"
    );
    let routes = crate::annotation::routes(&good_pool(), "gatewaytest.Acronyms").expect("passes");
    let text = crate::codegen::render("gatewaytest.Acronyms", &routes);
    assert!(text.contains("    GetHttpUsage,\n"), "{text}");
    assert!(
        text.contains("Self::GetHttpUsage => \"GetHTTPUsage\","),
        "{text}"
    );
    assert!(
        text.contains("decode_as::<::js_proto::v1::HttpRequest>(&input)?;"),
        "{text}"
    );
    assert!(
        text.contains("service.get_http_usage(::tonic::Request::new(request))"),
        "{text}"
    );
    assert!(text.contains("verb: Verb::Get,"), "{text}");
}

/// The pool of the test descriptor `gatewaytest/good.proto`.
fn good_pool() -> DescriptorPool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut compiler =
        protox::Compiler::new([root.join("tests/protos"), root.join("../js-proto/proto")])
            .expect("the include directories exist");
    compiler
        .open_file("gatewaytest/good.proto")
        .expect("the test descriptor compiles");
    compiler.descriptor_pool()
}

/// Asserts that a query sets a bool, a list of bools and a map of bools from `true` and
/// `false`, that other text for a bool is a 400 naming the field, and that a path variable
/// that is no id may hold any text but the empty one. Catches a bool read from text it does
/// not spell.
#[test]
fn a_query_sets_bools() {
    let request = good_pool()
        .get_message_by_name("gatewaytest.Req")
        .expect("defined");
    let query = "flag=true&flags=false&flags=true&switches[on]=true&switches[off]=false";
    let message =
        crate::input::build(&request, false, &[], Some(query), b"").expect("the query decodes");
    assert_eq!(
        message.get_field_by_name("flag").as_deref(),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        message.get_field_by_name("flags").as_deref(),
        Some(&Value::List(vec![Value::Bool(false), Value::Bool(true)]))
    );
    let switches = message
        .get_field_by_name("switches")
        .expect("the map is set");
    let switches = switches.as_map().expect("the field is a map");
    assert_eq!(switches.len(), 2);
    assert_eq!(
        switches.get(&MapKey::String("on".to_owned())),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        switches.get(&MapKey::String("off".to_owned())),
        Some(&Value::Bool(false))
    );

    let refused = crate::input::build(&request, false, &[], Some("flag=yes"), b"");
    assert_eq!(
        refused.map(|_| ()),
        Err(Answer::invalid(
            Some("flag".to_owned()),
            "the field is unknown, or does not hold a value of its type"
        ))
    );
    assert!(crate::input::path_value_ok("id", "x"));
    assert!(!crate::input::path_value_ok("id", ""));
}
