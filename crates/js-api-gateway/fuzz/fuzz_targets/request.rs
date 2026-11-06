//! Any bytes as the request of a route of the gateway, through the public router: the
//! decoder must not panic, must answer a 4xx with a known code or call the service, must
//! answer a body over the limit 413 without calling the service, must keep its live memory
//! under [`PEAK_LIMIT`] bytes above what it held when the request arrived (the allocator,
//! `cap::Cap` over the system allocator, has that bound as its limit while the request is
//! served, and an allocation past it fails, which the default allocation-error handler turns
//! into an abort that the fuzzer reports as a crash), and a request it
//! decodes must encode to a body that decodes to the same request.
//!
//! The first byte of the input picks the route and how the rest is used; see [`Mode`].

#![no_main]

use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use js_api_gateway::{BODY_LIMIT, router};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, GetJobRequest, GetJobResponse, ListJobsRequest,
    ListJobsResponse, SubmitJobRequest, SubmitJobResponse,
};
use libfuzzer_sys::fuzz_target;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use tonic::{Response, Status};
use tower::ServiceExt;

/// The most live bytes a request may add above what the process held when it arrived:
/// 256 times `BODY_LIMIT`, 16 MiB.
///
/// The body is at most `BODY_LIMIT` (64 KiB), but the decoder holds it as a JSON tree and
/// then copies the tree for the proto3 decoding, and a small JSON value costs far more than
/// its text: an object with one member is 8 input bytes and about 650 bytes of tree. The
/// worst body found is `{"x":[{"":0},{"":0},...]}`, an unknown member holding as many such
/// objects as fit: 65,107 input bytes peaked at 12,579,984 live bytes (192 times the body)
/// in a local run, and 150 seconds of fuzzing from that seed, from bodies of exactly
/// `BODY_LIMIT` and from arrays of as many small values as fit (`Mode::Wide`) found none
/// higher. The other bodies tried peaked lower: an array of `{"a":1}` at 5.4 MB, a valid
/// submit with 2,500 labels at 0.9 MB, `[0]` elements at 3.0 MB. The limit is that peak with
/// a third more room. It is a bound on the amplification, not a claim that it is small: a
/// decoder that read the body twice over, or buffered past `BODY_LIMIT`, would exceed it.
const PEAK_LIMIT: usize = 256 * BODY_LIMIT;

#[global_allocator]
static ALLOC: cap::Cap<std::alloc::System> = cap::Cap::new(std::alloc::System, usize::MAX);

/// What the service received: the submit request, if one came, and the number of calls.
#[derive(Default)]
struct Received {
    submit: Option<SubmitJobRequest>,
    calls: usize,
}

/// A [`JobService`] that records the submit request and counts the calls of four methods,
/// answering each with an empty response; every other method is the generated `UNIMPLEMENTED`.
#[derive(Default)]
struct Stub(Mutex<Received>);

impl Stub {
    fn record(&self, record: impl FnOnce(&mut Received)) {
        let mut received = self.0.lock().expect("the lock is not poisoned");
        received.calls += 1;
        record(&mut received);
    }
}

#[tonic::async_trait]
impl JobService for Stub {
    async fn submit_job(
        &self,
        request: tonic::Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        self.record(|r| r.submit = Some(request.into_inner()));
        Ok(Response::new(SubmitJobResponse::default()))
    }

    async fn cancel_job(
        &self,
        _: tonic::Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        self.record(|_| ());
        Ok(Response::new(CancelJobResponse::default()))
    }

    async fn get_job(
        &self,
        _: tonic::Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        self.record(|_| ());
        Ok(Response::new(GetJobResponse::default()))
    }

    async fn list_jobs(
        &self,
        _: tonic::Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        self.record(|_| ());
        Ok(Response::new(ListJobsResponse::default()))
    }
}

/// How the rest of the input is made into the payload.
enum Mode {
    /// As it is.
    Plain,
    /// Followed by spaces to exactly `BODY_LIMIT` bytes, so a large valid body can be found.
    Padded,
    /// The rest as the elements of an array, as many as fit in `BODY_LIMIT` bytes: a body of
    /// as many small JSON values as it can hold.
    Wide,
    /// The rest repeated to one byte over `BODY_LIMIT`.
    OverLimit,
}

/// The payload the rest of the input makes in `mode`.
fn payload_of(mode: &Mode, rest: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    match mode {
        Mode::Plain => payload.extend_from_slice(rest),
        Mode::Padded => {
            payload.extend_from_slice(rest);
            payload.resize(payload.len().max(BODY_LIMIT), b' ');
        }
        Mode::Wide if !rest.is_empty() => {
            payload.push(b'[');
            while payload.len() + rest.len() + 2 <= BODY_LIMIT {
                payload.extend_from_slice(rest);
                payload.push(b',');
            }
            payload.extend_from_slice(rest);
            payload.push(b']');
            payload.resize(BODY_LIMIT, b' ');
        }
        Mode::Wide => {}
        Mode::OverLimit => {
            if !rest.is_empty() {
                payload.extend(rest.iter().copied().cycle().take(BODY_LIMIT + 1));
            }
        }
    }
    payload
}

/// The runtime, the router and the stub, made once.
struct Harness {
    runtime: tokio::runtime::Runtime,
    router: Router,
    stub: Arc<Stub>,
    pool: DescriptorPool,
}

fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let stub = Arc::new(Stub::default());
        Harness {
            runtime: tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("the runtime starts"),
            router: router(stub.clone()),
            stub,
            pool: DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("decodes"),
        }
    })
}

/// `bytes` as URI text: letters, digits and the characters of a query kept, every other
/// byte escaped, so any bytes make a URI and a malformed escape (`%` kept) can be sent.
fn uri_text(bytes: &[u8]) -> String {
    let mut text = String::new();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"=&%+-_.~".contains(&byte) {
            text.push(char::from(byte));
        } else {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}

/// The answer to one request: the status and the JSON body.
fn send(
    harness: &Harness,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> Option<(StatusCode, serde_json::Value)> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body))
        .ok()?;
    let router = harness.router.clone();
    ALLOC
        .set_limit(ALLOC.allocated() + PEAK_LIMIT)
        .expect("the limit is above what is live");
    let response = harness
        .runtime
        .block_on(router.oneshot(request))
        .expect("the router answers");
    ALLOC
        .set_limit(usize::MAX)
        .expect("the limit is above what is live");
    let status = response.status();
    let bytes = harness
        .runtime
        .block_on(to_bytes(response.into_body(), usize::MAX))
        .expect("the body reads");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Some((status, json))
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let mode = match first >> 4 {
        0..=9 => Mode::Plain,
        10..=11 => Mode::Padded,
        12..=13 => Mode::Wide,
        _ => Mode::OverLimit,
    };
    let payload = payload_of(&mode, rest);
    let harness = harness();
    *harness.stub.0.lock().expect("the lock is not poisoned") = Received::default();
    let route = first & 0x0f;
    let (method, uri, body) = match route % 5 {
        0 => ("POST", "/v1/jobs".to_owned(), payload.clone()),
        1 => ("POST", "/v1/jobs/job-1:cancel".to_owned(), payload.clone()),
        2 => (
            "GET",
            format!("/v1/jobs?{}", uri_text(&payload)),
            Vec::new(),
        ),
        3 => (
            "GET",
            format!("/v1/jobs/job-1?{}", uri_text(&payload)),
            Vec::new(),
        ),
        _ => (
            "GET",
            format!("/v1/jobs/{}", uri_text(&payload[..payload.len().min(80)])),
            Vec::new(),
        ),
    };
    let sent = body.len();
    let Some((status, json)) = send(harness, method, &uri, body.clone()) else {
        return;
    };
    let received = harness.stub.0.lock().expect("the lock is not poisoned");
    if sent > BODY_LIMIT {
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(json["error"], "request-too-large");
        assert_eq!(received.calls, 0);
        return;
    }
    if received.calls > 0 {
        assert_eq!(received.calls, 1);
        assert!(status.is_success(), "{status}");
    } else {
        assert!(status.is_client_error(), "{status}");
        let code = json["error"].as_str().expect("the error is a code");
        assert!(
            [
                "invalid-argument",
                "request-too-large",
                "not-found",
                "method-not-allowed"
            ]
            .contains(&code),
            "{code}"
        );
    }
    if let Some(request) = &received.submit {
        // The request, written as JSON, is sent again and decodes to itself.
        let input = harness
            .pool
            .get_message_by_name("jobscheduler.v1.SubmitJobRequest")
            .expect("defined");
        let message = DynamicMessage::decode(input, request.encode_to_vec().as_slice())
            .expect("a generated message decodes");
        let again = serde_json::to_vec(&message).expect("a message serializes");
        let expected = request.clone();
        drop(received);
        *harness.stub.0.lock().expect("the lock is not poisoned") = Received::default();
        let (status, _) = send(harness, "POST", "/v1/jobs", again).expect("a request");
        assert!(status.is_success(), "{status}");
        let received = harness.stub.0.lock().expect("the lock is not poisoned");
        assert_eq!(received.submit.as_ref(), Some(&expected));
    }
});
