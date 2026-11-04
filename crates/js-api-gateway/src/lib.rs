//! The HTTP/JSON gateway of the job service: routes under `/v1/` generated from the
//! `google.api.http` option of each method of `jobscheduler.v1.JobService` (crate
//! `js-proto`), which call the service's generated trait
//! [`JobService`](js_proto::v1::job_service_server::JobService). No route and no JSON field
//! is written by hand: the build script reads the options from js-proto's descriptor set,
//! checks them ([`annotation`]) and generates [`ROUTES`], [`Rpc`] and the call of each
//! method, and the messages are mapped to and from JSON from their descriptors. A method of
//! the service appears over HTTP when it is annotated; an unannotated one has no route.
//!
//! The crate is a library and serves nothing by itself: [`router`] is mounted by a server,
//! which brings the [`JobService`](js_proto::v1::job_service_server::JobService) behind it.
//! It depends on no server crate.
//!
//! # Routes
//!
//! [`ROUTES`] is the table the router is built from, in the order of the service
//! definition. It prints as the table of `docs/http-gateway.md`:
//!
//! ```
//! for route in js_api_gateway::ROUTES {
//!     println!("{} {} -> {}", route.verb.as_str(), route.template, route.rpc.name());
//! }
//! assert_eq!(js_api_gateway::ROUTES.len(), 12);
//! ```
//!
//! A call that changes state is a POST and a read is a GET. `CancelJob` is
//! `POST /v1/jobs/{job_id}:cancel`, not a `DELETE`: a cancel keeps the job's record and its
//! result, where a `DELETE` would read as removal. It is idempotent on the job, so a repeat
//! answers 200 with the same final state. The router has one axum route per resource
//! (`/v1/jobs`, `/v1/hosts`, ...) and below it, and matches a request against the routes'
//! templates itself, so that a custom verb (`:cancel`) and an empty path segment are
//! matched as the templates say. It matches the whole path, so it is merged at the root, not
//! nested; its resource routes (`/v1/hosts` and every path below it, and so on) overlap the
//! lease server's routes under the same resources, which a server mounting both has to
//! resolve (`docs/http-gateway.md`).
//!
//! # Requests
//!
//! A path variable sets the request field it names. A POST route takes the request message
//! as its JSON body (an empty body is `{}`) and no query string. A GET route takes no body;
//! its query string sets the other fields: `name=value` for a singular field (once),
//! repeated for a list (`states=JOB_STATE_QUEUED&states=JOB_STATE_RUNNING`), and
//! `name[key]=value` for each entry of a map (`labels[team]=ml`). A field may be named by
//! its JSON name or its proto name.
//!
//! A `job_id` (1 to 63 bytes of letters, digits, dot, underscore and dash, design section
//! 5.3 and `job.proto`) is checked before the call, in the path, the body and the query
//! (outside lists and maps); an empty one in a body or query is left to the service (an
//! empty job id in a submit asks the service to mint one). Any other path variable, such as
//! a `host_id`, must not be empty; the service judges the rest.
//!
//! A call that succeeds answers 200 with its response, or the status the method's
//! `jobscheduler.v1.http_success_status` option names: `SubmitJob` answers 201, also for a
//! repeated submit, which answers the same body.
//!
//! # JSON mapping
//!
//! Bodies are the proto3 JSON mapping of the messages, as the gateway's documented choice:
//! fields by their lowerCamelCase JSON names (a request may also use the proto names),
//! enums by their value names (`"JOB_STATE_RUNNING"`), 64-bit integers as decimal strings
//! (`"9007199254740993"`, so no JSON number rounds them), bytes as base64, and fields at
//! their default values left out of a response. A request with a field the message does
//! not have is refused (a 400 naming it), so an older server tells a newer client that it
//! does not know the field rather than dropping it.
//!
//! # Limits
//!
//! A request body over [`BODY_LIMIT`] (64 KiB, the lease server's body limit) is a 413
//! `request-too-large` before anything is decoded. The gateway adds no buffer of its own
//! for the request head: the listener it is mounted behind bounds the head, query string
//! included.
//!
//! # Authentication
//!
//! The router carries no token logic and authenticates nothing. The server that mounts it
//! puts its bearer layer in front, which answers an unauthenticated request 401 with an
//! empty body before the gateway sees it.
//!
//! # Errors
//!
//! An answer that is not the call's response is an [`Answer`]: the status and
//! `{"error": "<code>"}`, plus `"field"` (the first offending field, by its JSON name,
//! `spec.jobId` inside the submitted job) and `"reason"` (a fixed text) on a 400. Neither
//! ever repeats a value of the request, and a 500 never says its cause.
//!
//! | answer | status | code |
//! |---|---|---|
//! | a request the gateway cannot decode: malformed JSON, an unknown field, a value of the wrong kind, a bad id | 400 | `invalid-argument`, with `field` |
//! | a body over [`BODY_LIMIT`] | 413 | `request-too-large` |
//! | no route matches the path under a resource of the gateway | 404 | `not-found` |
//! | a route matches the path for another method only (with `Allow`) | 405 | `method-not-allowed` |
//! | refusal `job-id-conflict` (same id, different job) | 409 | `job-id-conflict` |
//! | refusal `no-capable-host` at admission | 422 | `no-capable-host` |
//! | refusal `wall-time-not-positive` | 400 | `wall-time-not-positive`, `field` `spec.wallLimitSeconds` |
//! | refusal `wall-time-above-ceiling` | 400 | `wall-time-above-ceiling`, `field` `spec.wallLimitSeconds` |
//! | `NOT_FOUND`, message `unknown-job` | 404 | `unknown-job` |
//! | `NOT_FOUND`, message `expired` (result or logs past retention) | 410 | `expired` |
//! | `FAILED_PRECONDITION`, message `not-finished` (the result of a job that has not ended) | 409 | `not-finished` |
//! | `INVALID_ARGUMENT` (a message `field: <name>` gives `field`) | 400 | `invalid-argument` |
//! | `UNIMPLEMENTED` | 501 | `unimplemented` |
//! | `INTERNAL`, message `state-write-failed` (a store write that failed) | 500 | `state-write-failed` |
//! | any other status, an unknown refusal, a fault of the gateway | 500 | `internal` |
//! | unauthenticated (the mounting server's bearer layer) | 401 | empty body |
//!
//! A status that carries a refusal ([`js_proto::refusal`]) is answered by its refusal code
//! ([`Answer::of_refusal`]); any other by its gRPC code and its message, which the service
//! sets to the code of the table ([`Answer::of_status`]). Both are one `match` with no
//! wildcard arm over the codes, so a refusal code added to the service fails the build
//! until its row is chosen.

use std::sync::Arc;

use axum::Router;
use js_proto::v1::job_service_server::JobService;
use prost_reflect::{DynamicMessage, MessageDescriptor};
use tonic::Status;

pub mod annotation;
mod answer;
mod input;
mod serve;

pub use annotation::Verb;
pub use answer::Answer;

include!(concat!(env!("OUT_DIR"), "/gateway.rs"));

/// The largest request body the gateway reads, in bytes: 64 KiB, the body limit of the
/// lease server's routes.
pub const BODY_LIMIT: usize = 64 * 1024;

/// One route of the gateway, as the build script generated it from a method's
/// `google.api.http` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The HTTP method.
    pub verb: Verb,
    /// The path template as the option spells it, such as `/v1/jobs/{job_id}:cancel`.
    pub template: &'static str,
    /// The method the route calls.
    pub rpc: Rpc,
    /// The body is the request message (`body: "*"`). Otherwise the route takes no body
    /// and its query string sets the fields its path does not.
    pub body: bool,
    /// The HTTP status of a success: 200, or 201 for a method whose
    /// `http_success_status` option says so (`SubmitJob`).
    pub status: u16,
    /// The full name of the request message, such as `jobscheduler.v1.GetJobRequest`.
    pub input: &'static str,
    /// The full name of the response message.
    pub output: &'static str,
}

/// The gateway's router over `service`: every route of [`ROUTES`], and below each resource
/// path (such as `/v1/jobs`) nothing else (a 404 or a 405). It authenticates nothing; see
/// the crate documentation.
pub fn router<T: JobService>(service: Arc<T>) -> Router {
    serve::router_over(service, js_proto::FILE_DESCRIPTOR_SET)
}

/// The request message `input` as the method's generated request type `Req`. A message that
/// does not convert is a 500 `internal`.
pub(crate) fn decode_as<Req: prost::Message + Default>(
    input: &DynamicMessage,
) -> Result<Req, Answer> {
    input.transcode_to::<Req>().map_err(conversion_failed)
}

/// The answer to a call: its response, encoded, as a message of `output`, or the answer to
/// its status ([`Answer::of_status`]). A response that does not decode as `output` is a 500
/// `internal`.
pub(crate) fn answer_of(
    output: &MessageDescriptor,
    response: Result<Vec<u8>, Status>,
) -> Result<DynamicMessage, Answer> {
    let bytes = response.map_err(|status| Answer::of_status(&status))?;
    let message = DynamicMessage::decode(output.clone(), bytes.as_slice());
    message.map_err(conversion_failed)
}

/// The answer to a message that does not convert between a descriptor and a generated type.
fn conversion_failed(_: prost::DecodeError) -> Answer {
    Answer::internal()
}

#[cfg(test)]
mod tests;
