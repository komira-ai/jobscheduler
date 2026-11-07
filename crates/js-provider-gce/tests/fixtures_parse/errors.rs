//! The error answers of the Compute Engine and Cloud Billing APIs.

use super::*;
use crate::launch::{by_id, insert_request};

fn refused(code: RefusalCode, reason: &str) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api(reason.to_owned()),
    })
}

fn error_of(kind: fn(ErrorReason) -> ApiError, reason: &str) -> ApiError {
    kind(ErrorReason::new(reason))
}

fn throttled(reason: ErrorReason) -> ApiError {
    ApiError::Throttled { reason }
}

fn denied(reason: ErrorReason) -> ApiError {
    ApiError::Denied { reason }
}

fn refused_error(reason: ErrorReason) -> ApiError {
    ApiError::Refused { reason }
}

/// Asserts, for each error answer of the Compute Engine API, that an `instances.insert`
/// that gets it fails with exactly the `ApiError` the answer's HTTP status and first
/// `errors[].reason` make, and that the launch answer for that error is the one the
/// acquire acts on:
///
/// - 404 `notFound` and 400 `invalid` are refused as a shape error;
/// - 403 `forbidden`, 401 `authError` and, because every other 403 is read as a denial,
///   403 `quotaExceeded` are refused as denied (whether that 403 should be a quota refusal
///   is komira-ai/jobscheduler#768; this row pins today's mapping until it is decided);
/// - 403 and 429 `rateLimitExceeded` are throttles, and 409 `alreadyExists`, 400
///   `badRequest`, 503 `backendError` and 500 `internalError` are plain refusals; all five
///   leave the launch unknown, since the insert may have landed.
///
/// It catches a throttle read as a refusal, a server error read as a denial, and an
/// answer whose first reason is not the one read.
#[test]
fn every_compute_error_answer_is_the_api_error_and_launch_answer_its_reason_makes() {
    let cases: [(&str, u16, &str, ApiError, Acquired); 11] = [
        (
            fixture!("error_not_found_404.json"),
            404,
            "error_not_found_404.json",
            error_of(refused_error, "notFound"),
            refused(RefusalCode::Shape, "notFound"),
        ),
        (
            fixture!("error_already_exists_409.json"),
            409,
            "error_already_exists_409.json",
            error_of(refused_error, "alreadyExists"),
            Acquired::Unknown,
        ),
        (
            fixture!("error_rate_limit_exceeded_403.json"),
            403,
            "error_rate_limit_exceeded_403.json",
            error_of(throttled, "rateLimitExceeded"),
            Acquired::Unknown,
        ),
        (
            fixture!("error_rate_limit_exceeded_429.json"),
            429,
            "error_rate_limit_exceeded_429.json",
            error_of(throttled, "rateLimitExceeded"),
            Acquired::Unknown,
        ),
        (
            fixture!("error_quota_exceeded_403.json"),
            403,
            "error_quota_exceeded_403.json",
            error_of(denied, "quotaExceeded"),
            refused(RefusalCode::Denied, "quotaExceeded"),
        ),
        (
            fixture!("error_forbidden_403.json"),
            403,
            "error_forbidden_403.json",
            error_of(denied, "forbidden"),
            refused(RefusalCode::Denied, "forbidden"),
        ),
        (
            fixture!("error_auth_error_401.json"),
            401,
            "error_auth_error_401.json",
            error_of(denied, "authError"),
            refused(RefusalCode::Denied, "authError"),
        ),
        (
            fixture!("error_invalid_400.json"),
            400,
            "error_invalid_400.json",
            error_of(refused_error, "invalid"),
            refused(RefusalCode::Shape, "invalid"),
        ),
        (
            fixture!("error_bad_request_400.json"),
            400,
            "error_bad_request_400.json",
            error_of(refused_error, "badRequest"),
            Acquired::Unknown,
        ),
        (
            fixture!("error_backend_error_503.json"),
            503,
            "error_backend_error_503.json",
            error_of(refused_error, "backendError"),
            Acquired::Unknown,
        ),
        (
            fixture!("error_internal_error_500.json"),
            500,
            "error_internal_error_500.json",
            error_of(refused_error, "internalError"),
            Acquired::Unknown,
        ),
    ];
    for (text, code, name, error, launch) in cases {
        let (client, script) = over(vec![status(code, text)]);
        let got = client.insert_instance(&insert_request()).unwrap_err();
        assert_eq!(got, error, "{name}");
        assert_eq!(launch_answer(&got), launch, "{name}");
        assert_eq!(api_calls(&script).len(), 1, "{name}");
    }
}

/// Asserts that a 404 `notFound` answer to `instances.get` and to `instances.delete` is
/// the error that says the instance does not exist, and that the 409 `alreadyExists`
/// answer to an insert is the error that says the name is taken; no other fixture answer
/// is either. It catches the evidence of "gone" being any other failure.
#[test]
fn only_the_documented_reasons_say_not_found_and_already_exists() {
    let (client, _) = over(vec![status(404, fixture!("error_not_found_404.json"))]);
    let error = client.get_instance(&by_id(INSTANCE_ID)).unwrap_err();
    assert!(error.is_not_found());
    assert!(!error.is_already_exists());
    let (client, _) = over(vec![status(404, fixture!("error_not_found_404.json"))]);
    assert!(
        client
            .delete_instance(&by_id(INSTANCE_ID))
            .unwrap_err()
            .is_not_found()
    );
    let (client, _) = over(vec![status(409, fixture!("error_already_exists_409.json"))]);
    let error = client.insert_instance(&insert_request()).unwrap_err();
    assert!(error.is_already_exists());
    assert!(!error.is_not_found());
    let (client, _) = over(vec![status(503, fixture!("error_backend_error_503.json"))]);
    let error = client.get_instance(&by_id(INSTANCE_ID)).unwrap_err();
    assert!(!error.is_not_found());
}

/// Asserts that the Cloud Billing errors, which carry a `google.rpc` status and no
/// `errors` list, are read from the status: 403 `PERMISSION_DENIED` is a denial and 429
/// `RESOURCE_EXHAUSTED` is a throttle under the reason `RATE_LIMIT_EXCEEDED` (the HTTP
/// status makes it one; the `ErrorInfo` reason in `details` is the same text). Through the
/// catalog the failure names the call and the reason, and is never an empty catalog; the
/// launch answers are a denial refusal and unknown. It catches a Billing error read as an
/// answer with no reason and a throttle read as a plain refusal.
#[test]
fn cloud_billing_errors_are_read_from_their_status() {
    let case = |code: u16, text: &str| {
        let (client, _) = over(vec![status(code, text)]);
        client.prices(&region()).unwrap_err()
    };
    let denial = case(403, fixture!("error_billing_permission_denied_403.json"));
    assert_eq!(denial, error_of(denied, "PERMISSION_DENIED"));
    assert_eq!(
        launch_answer(&denial),
        refused(RefusalCode::Denied, "PERMISSION_DENIED")
    );
    let slow = case(429, fixture!("error_billing_resource_exhausted_429.json"));
    assert_eq!(slow, error_of(throttled, "RATE_LIMIT_EXCEEDED"));
    assert_eq!(launch_answer(&slow), Acquired::Unknown);

    for (code, text, message) in [
        (
            403,
            fixture!("error_billing_permission_denied_403.json"),
            "services.skus.list failed: denied (PERMISSION_DENIED)",
        ),
        (
            429,
            fixture!("error_billing_resource_exhausted_429.json"),
            "services.skus.list failed: throttled (RATE_LIMIT_EXCEEDED)",
        ),
    ] {
        let mut answers = machine_pages();
        answers.push(answer(fixture!("accelerator_types_aggregated.json")));
        answers.push(status(code, text));
        let (client, script) = over(answers);
        let settings = GceCatalogSettings::new(
            PROJECT,
            &[ZONE_A, ZONE_B],
            &["on-demand"],
            Some(vec!["n1-standard-4".to_owned()]),
        )
        .unwrap();
        let error = GceCatalog::new(Arc::new(client), settings)
            .catalog()
            .unwrap_err();
        assert_eq!(error.to_string(), message);
        assert_eq!(script.left(), 0);
    }
}
