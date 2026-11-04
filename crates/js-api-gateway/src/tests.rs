//! Tests of the crate's private paths: messages that do not convert between a descriptor and
//! a generated type, a router over an in-crate service (whose instantiation of the router's
//! generic code runs here, apart from the integration tests' stub), a router made from a
//! descriptor set that lacks the routes' messages, and the query values only a test
//! descriptor has (bools).

use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{GetJobRequest, GetLogsResponse, JobState, ListHostsRequest, ListHostsResponse};
use prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MapKey, Value};
use tonic::Status;
use tower::ServiceExt;

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

/// The status and body the router over `descriptors` answers to `GET uri`.
async fn get(descriptors: &[u8], uri: &str) -> (StatusCode, Vec<u8>) {
    let router = crate::serve::router_over(Arc::new(Hosts), descriptors);
    let request = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("the request is well formed");
    let response = router.oneshot(request).await.expect("the router answers");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read");
    (status, body.to_vec())
}

/// Asserts that a router made from a descriptor set that does not decode, and from one that
/// decodes but lacks the routes' messages, answers 500 `internal`; and that with the
/// service's descriptors it answers a call that succeeds (200), one the service does not
/// implement (501), one with a bad id (400) and a path no route has (404). Catches a router
/// that panics or serves without its messages.
#[tokio::test]
async fn a_router_answers_from_its_messages_only() {
    for descriptors in [&b"\xff"[..], &[][..]] {
        let (status, body) = get(descriptors, "/v1/jobs/job-1").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, br#"{"error":"internal"}"#);
    }
    let set = js_proto::FILE_DESCRIPTOR_SET;
    assert_eq!(
        get(set, "/v1/hosts").await,
        (StatusCode::OK, b"{}".to_vec())
    );
    let (status, body) = get(set, "/v1/jobs/job-1").await;
    assert_eq!(
        (status, body),
        (
            StatusCode::NOT_IMPLEMENTED,
            br#"{"error":"unimplemented"}"#.to_vec()
        )
    );
    let (status, _) = get(set, "/v1/jobs/bad~id").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(set, "/v1/jobs/job-1/nothing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
