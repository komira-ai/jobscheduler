//! Tests of the crate's private paths: a call whose messages do not convert, a router made
//! from a descriptor set that lacks the routes' messages, and the query values only a test
//! descriptor has (bools).

use std::future::{Ready, ready};
use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{GetJobRequest, GetLogsResponse, JobState};
use prost_reflect::{DescriptorPool, DynamicMessage, MapKey, Value};
use tonic::Status;
use tower::ServiceExt;

use crate::answer::Answer;

/// The pool of the job service.
fn service_pool() -> DescriptorPool {
    DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// An answer of the type `GetLogsResponse` to a `GetJob` call: the wrong type for the
/// method, so its response does not convert to a `GetJobResponse`.
fn logs(
    _: tonic::Request<GetJobRequest>,
) -> Ready<Result<tonic::Response<GetLogsResponse>, Status>> {
    ready(Ok(tonic::Response::new(GetLogsResponse {
        state: JobState::Running.into(),
        ..GetLogsResponse::default()
    })))
}

/// Asserts that a request message that does not convert to the method's request type, and
/// a response that does not convert to the route's response message, are each a 500
/// `internal` (a varint where a string is), the first without calling the method. Catches
/// a conversion fault answered as anything but an internal error.
#[tokio::test]
async fn a_message_that_does_not_convert_is_internal() {
    let pool = service_pool();
    let logs_descriptor = pool
        .get_message_by_name("jobscheduler.v1.GetLogsResponse")
        .expect("defined");
    let job_request = pool
        .get_message_by_name("jobscheduler.v1.GetJobRequest")
        .expect("defined");
    let mut running = DynamicMessage::new(logs_descriptor);
    running.set_field_by_name("state", Value::EnumNumber(JobState::Running.into()));

    let refused = crate::unary(running, &job_request, logs).await;
    assert_eq!(refused, Err(Answer::internal()));

    let empty = DynamicMessage::new(job_request.clone());
    let refused = crate::unary(empty, &job_request, logs).await;
    assert_eq!(refused, Err(Answer::internal()));
}

/// A service with no method of its own: every call is the generated `UNIMPLEMENTED`.
struct Nothing;

#[tonic::async_trait]
impl JobService for Nothing {}

/// Asserts that a router made from a descriptor set that does not decode, and from one that
/// decodes but lacks the routes' messages, answers every request 500 `internal` and still
/// 404 outside its resources. Catches a router that panics or serves without its messages.
#[tokio::test]
async fn a_router_without_its_messages_is_internal() {
    for descriptors in [&b"\xff"[..], &[][..]] {
        let router = crate::serve::router_over(Arc::new(Nothing), descriptors);
        let request = Request::builder()
            .uri("/v1/jobs/job-1")
            .body(Body::empty())
            .expect("the request is well formed");
        let response = router.oneshot(request).await.expect("the router answers");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read");
        assert_eq!(&body[..], br#"{"error":"internal"}"#);
    }
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
