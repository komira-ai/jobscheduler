//! What every call shares: the bearer token, how an error answer is told, how a transport
//! failure is told, how a success that is not JSON is told, and the walk over pages.

use std::io;

use js_logstore::TransportError;
use js_provider_gce::{ApiError, ErrorReason, GceApi};

use crate::launch::{ZONE, by_name, instance_json, project, zone};
use crate::script::{answer, api_calls, error_body, over, status};

fn reason(text: &str) -> ErrorReason {
    ErrorReason::new(text)
}

fn throttled(text: &str) -> ApiError {
    ApiError::Throttled {
        reason: reason(text),
    }
}

fn denied(text: &str) -> ApiError {
    ApiError::Denied {
        reason: reason(text),
    }
}

fn refused(text: &str) -> ApiError {
    ApiError::Refused {
        reason: reason(text),
    }
}

/// The error `get_instance` returns for one scripted answer.
fn error_of(answered: crate::script::Answer) -> ApiError {
    let (client, script) = over(vec![answered]);
    let error = client.get_instance(&by_name()).unwrap_err();
    assert_eq!(api_calls(&script).len(), 1, "a call is made once");
    assert_eq!(script.left(), 0);
    assert!(!format!("{error:?} {error}").contains("private text"));
    error
}

/// Asserts that an error answer is told by the reason of its first error, whatever the
/// HTTP status: a rate limit reason is a throttle (also at 403), a `forbidden` or
/// `PERMISSION_DENIED` reason a denial, `notFound` and `alreadyExists` reasons are
/// refusals the error methods recognise, and any other reason is a refusal; and that the
/// message of the answer is in no error text. It catches an error mapped from the status
/// alone, and one that keeps the cloud's message.
#[test]
fn an_error_answer_is_told_by_its_reason() {
    assert_eq!(
        error_of(status(403, &error_body(403, "rateLimitExceeded"))),
        throttled("rateLimitExceeded")
    );
    assert_eq!(
        error_of(status(429, &error_body(429, "rateLimitExceeded"))),
        throttled("rateLimitExceeded")
    );
    assert_eq!(
        error_of(status(500, &error_body(500, "backendError"))),
        refused("backendError")
    );
    assert_eq!(
        error_of(status(400, &error_body(400, "PERMISSION_DENIED"))),
        denied("PERMISSION_DENIED")
    );
    let missing = error_of(status(404, &error_body(404, "notFound")));
    assert_eq!(missing, refused("notFound"));
    assert!(missing.is_not_found());
    let taken = error_of(status(409, &error_body(409, "alreadyExists")));
    assert_eq!(taken, refused("alreadyExists"));
    assert!(taken.is_already_exists());
}

/// Asserts that HTTP 401 is always a denial, with the answer's reason when it has one
/// (`authError`, or the `error.status`) and `UNAUTHENTICATED` when it has none or the body
/// is not JSON; and that HTTP 403 is a denial with the answer's reason, or `forbidden` when
/// it has none (a 403 `quotaExceeded` is the one refusal, asserted apart). It catches a rejected credential that is read as a refusal the scheduler
/// might retry.
#[test]
fn a_401_and_a_403_are_denials() {
    assert_eq!(
        error_of(status(401, &error_body(401, "authError"))),
        denied("authError")
    );
    assert_eq!(
        error_of(status(401, "{\"error\":{\"status\":\"UNAUTHENTICATED\"}}")),
        denied("UNAUTHENTICATED")
    );
    assert_eq!(
        error_of(status(401, "{\"error\":{\"code\":401}}")),
        denied("UNAUTHENTICATED")
    );
    assert_eq!(error_of(status(401, "<html>")), denied("UNAUTHENTICATED"));
    assert_eq!(
        error_of(status(401, &error_body(401, "notFound"))),
        denied("notFound")
    );
    assert_eq!(
        error_of(status(403, &error_body(403, "forbidden"))),
        denied("forbidden")
    );
    assert_eq!(
        error_of(status(403, &error_body(403, "accessNotConfigured"))),
        denied("accessNotConfigured")
    );
    assert_eq!(error_of(status(403, "{}")), denied("forbidden"));
}

/// Asserts that an answer whose reason is `quotaExceeded` is a refusal with that reason,
/// at HTTP 403 (the status Compute Engine gives it) and at any other status, and that the
/// match is on the exact reason: a 403 whose reason differs only in case is still a denial.
/// It catches a project's quota answer read as a credential the cloud refuses, which makes
/// the scheduler treat the provider as denied instead of passing over the machine type.
#[test]
fn a_quota_exceeded_answer_is_a_refusal_not_a_denial() {
    assert_eq!(
        error_of(status(403, &error_body(403, "quotaExceeded"))),
        refused("quotaExceeded")
    );
    assert_eq!(
        error_of(status(400, &error_body(400, "quotaExceeded"))),
        refused("quotaExceeded")
    );
    assert_eq!(
        error_of(status(403, &error_body(403, "quotaexceeded"))),
        denied("quotaexceeded")
    );
}

/// Asserts that HTTP 429 is a throttle whatever the answer says, with `RATE_LIMIT_EXCEEDED`
/// when the answer names no throttle reason (none, another reason, or a body that is not
/// JSON). It catches a 429 that is read as an ordinary refusal.
#[test]
fn a_429_is_a_throttle() {
    let expected = throttled("RATE_LIMIT_EXCEEDED");
    assert_eq!(error_of(status(429, "")), expected);
    assert_eq!(
        error_of(status(429, &error_body(429, "backendError"))),
        expected
    );
    assert_eq!(
        error_of(status(
            429,
            "{\"error\":{\"status\":\"RESOURCE_EXHAUSTED\"}}"
        )),
        expected
    );
}

/// Asserts that the `error.status` is the reason when the first error has none, that an
/// error answer with no reason at all (no error object, no reasons, an empty reason, a
/// body that is not JSON) is `Unreadable` naming the HTTP status, and that none of these
/// keeps text of the answer. It catches an error answer that is guessed at.
#[test]
fn an_error_answer_with_no_reason_is_unreadable() {
    assert_eq!(
        error_of(status(
            503,
            "{\"error\":{\"errors\":[{\"message\":\"private text\"}],\"status\":\"UNAVAILABLE\"}}"
        )),
        refused("UNAVAILABLE")
    );
    let cases = [
        (
            500,
            "{\"error\":{\"code\":500,\"message\":\"private text\"}}",
        ),
        (500, "{\"message\":\"private text\"}"),
        (500, "{\"error\":{\"errors\":[],\"status\":\"\"}}"),
        (500, "{\"error\":{\"errors\":[{\"reason\":\"\"}]}}"),
        (500, "<html>private text</html>"),
        (500, ""),
    ];
    for (code, body) in cases {
        assert_eq!(
            error_of(status(code, body)),
            ApiError::Unreadable("an error answer with no reason (HTTP 500)".to_owned()),
            "{body}"
        );
    }
}

/// Asserts that a transport that timed out is `Timeout`, any other transport failure is
/// `Transport` with the client's own text of it, and neither is retried. It catches a
/// timeout that is read as a failure that changed nothing.
#[test]
fn a_transport_failure_is_a_timeout_or_a_transport_error() {
    assert_eq!(error_of(Err(TransportError::Timeout)), ApiError::Timeout);
    let reset = TransportError::Io(io::ErrorKind::ConnectionReset);
    let text = reset.to_string();
    assert_eq!(error_of(Err(reset)), ApiError::Transport(text));
}

/// Asserts that any 2xx answer is a success, and that a success whose body is not JSON is
/// `Unreadable`. It catches a client that takes only 200 for a success, and one that reads
/// an empty body as an empty answer.
#[test]
fn a_success_is_any_2xx_and_must_be_json() {
    let body = instance_json("1234567890123456789", "js-vm-one", "RUNNING", ZONE).to_string();
    let (client, _) = over(vec![status(201, &body)]);
    assert!(client.get_instance(&by_name()).is_ok());
    let (client, _) = over(vec![answer("")]);
    assert_eq!(
        client.get_instance(&by_name()),
        Err(ApiError::Unreadable("the answer is not JSON".to_owned()))
    );
}

/// Asserts that a machine type or zone URL is read as its last segment and a name with no
/// `/` is read as it is. It catches a URL kept whole.
#[test]
fn a_resource_url_is_read_as_its_last_segment() {
    let mut body = instance_json("1234567890123456789", "js-vm-one", "RUNNING", ZONE);
    body["machineType"] = "q9-standard-2".into();
    body["zone"] = ZONE.into();
    let (client, _) = over(vec![answer(&body.to_string())]);
    let instance = client.get_instance(&by_name()).unwrap();
    assert_eq!(instance.machine_type.as_str(), "q9-standard-2");
    assert_eq!(instance.zone, zone(ZONE));
}

/// Asserts that a list read in pages sends the token of each page, stops when no token
/// follows, and fails with `Incomplete` when the cloud answers the token it was just
/// given. It catches a walk that loops for ever and one that ends short in silence.
#[test]
fn a_list_is_walked_page_by_page_and_never_ends_short() {
    let page = |token: &str| answer(&format!("{{\"items\":{{}},\"nextPageToken\":\"{token}\"}}"));
    let zones = [zone(ZONE)];
    let (client, script) = over(vec![page("page-2"), answer("{\"items\":{}}")]);
    assert_eq!(client.machine_types(&project(), &zones), Ok(Vec::new()));
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].url.contains("pageToken"));
    assert!(calls[1].url.contains("pageToken=page-2"));
    let (client, script) = over(vec![page("page-2"), page("page-2")]);
    assert_eq!(
        client.machine_types(&project(), &zones),
        Err(ApiError::Incomplete)
    );
    assert_eq!(api_calls(&script).len(), 2);
}

/// Asserts that the debug text of the client names the type and nothing else. It catches a
/// debug text that prints the credential or the transport.
#[test]
fn the_debug_text_of_the_client_names_nothing() {
    let (client, _) = over(Vec::new());
    assert_eq!(format!("{client:?}"), "HttpGce { .. }");
}
