//! `SubmitJob` of a job that carries a retry policy (`spec.retry`) or a completion callback
//! (`spec.callback`), over both transports through the mounted handler: this release takes
//! neither, so it refuses both, before it looks the job's id up and before admission, and
//! records nothing (`docs/design/job-retry-and-completion-callback.md`, section 7). It delivers
//! the callback of a job a later release took; its submit takes one only where a test turns
//! the refusal off (`callback_tests.rs`).

use super::*;

use js_jobs::Retention;
use js_state::ManualClock;
use js_types::RetryPolicy;

use crate::agent_api::job_rig::{OneClock, START_MS, tables_on};
use crate::clock::SharedClock;
use crate::job_api::tests::token;

/// A callback whose URL carries the receiver's reference and whose key is 32 bytes, so a
/// refusal that repeated either would show it.
fn callback() -> v1::Callback {
    v1::Callback {
        url: "https://hooks.example/js-done/secret-ref".to_owned(),
        signing_key: b"k3y-k3y-k3y-k3y-k3y-k3y-k3y-k3y!".to_vec(),
        deadline_seconds: Some(60),
    }
}

/// A policy of `max_attempts` with both waits given.
fn policy(max_attempts: u32) -> v1::RetryPolicy {
    v1::RetryPolicy {
        max_attempts,
        initial_backoff_seconds: Some(5),
        max_backoff_seconds: Some(600),
    }
}

/// The 400 body the gateway answers for `field` refused with `code`.
fn refused_body(field: &str, code: &str) -> Value {
    json!({
        "error": "invalid-argument",
        "field": format!("{field}: {code}"),
        "reason": "the service refused an argument of the request",
    })
}

/// Asserts that a job carrying a retry policy (empty, of one attempt, which means no retry, or
/// of the most attempts) is refused `field: spec.retry: retry-not-enabled`, one carrying a
/// callback (empty, or with a URL, a key and a deadline) `field: spec.callback:
/// callback-not-enabled`, and one carrying both the policy's refusal: `INVALID_ARGUMENT` with
/// no `Refusal` detail over gRPC, and over JSON the whole 400 body naming the field and the
/// code and nothing else, so neither the URL nor the key is in any answer. The admission
/// would refuse every job and is never reached, and the tables stay empty. Catches a policy or
/// callback recorded and then dropped, so the job runs once and calls nothing back while its
/// client believes otherwise, a refusal that echoes the URL or the key, and a refusal that
/// comes after admission.
#[tokio::test]
async fn submit_refuses_retry_and_callback_until_enabled() {
    let (_, writes) = over_fake(Fixed::new(false)).await;
    let retry = ("spec.retry", "retry-not-enabled");
    let called = ("spec.callback", "callback-not-enabled");
    let cases = [
        (Some(v1::RetryPolicy::default()), None, retry),
        (Some(policy(1)), None, retry),
        (Some(policy(u32::MAX)), None, retry),
        (None, Some(v1::Callback::default()), called),
        (None, Some(callback()), called),
        (Some(policy(3)), Some(callback()), retry),
    ];
    for (index, (retry, callback, (field, code))) in cases.into_iter().enumerate() {
        let spec = v1::JobSpec {
            retry,
            callback,
            ..wire(&format!("asks-{index}"))
        };
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{index}");
        assert_eq!(
            status.message(),
            format!("field: {field}: {code}"),
            "{index}"
        );
        assert_eq!(refusal_of(&status), None, "{index}");
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body),
            (StatusCode::BAD_REQUEST, refused_body(field, code)),
            "{index}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}

/// Asserts that a job already recorded without a policy or a callback, submitted again with
/// one, is refused as a new one would be, not answered from the recorded job, and that the
/// recorded job is unchanged. Catches a resubmit that is answered from the stored job (whose
/// answer shows no policy) before the field is looked at, so a client would read a retry it
/// asked for as dropped by an older server.
#[tokio::test]
async fn a_resubmit_that_adds_a_policy_or_a_callback_is_refused_before_the_lookup() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let recorded = writes.submit_grpc(&wire("held-1")).await.unwrap();
    for (retry, callback, field, code) in [
        (Some(policy(2)), None, "spec.retry", "retry-not-enabled"),
        (
            None,
            Some(callback()),
            "spec.callback",
            "callback-not-enabled",
        ),
    ] {
        let spec = v1::JobSpec {
            retry,
            callback,
            ..wire("held-1")
        };
        for via in Via::BOTH {
            let (_, body) = writes.submit(via, &spec).await.unwrap_err();
            assert_eq!(body, refused_body(field, code), "{via:?}");
        }
    }
    assert_eq!(rows(&writes.jobs), 1);
    assert_eq!(writes.submit_grpc(&wire("held-1")).await.unwrap(), recorded);
}

/// The service over a fake store whose submit takes a retry policy
/// ([`Ctx::accepting_retry`]), admitting every job; the source of the admission with it.
async fn accepting() -> (Arc<Fixed>, Writes) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let admission = Fixed::new(true);
    let ctx = Ctx::served(CEILING, admission.clone()).accepting_retry();
    let shared = SharedClock::new(OneClock(clock));
    let api = JobApi::new(Arc::new(jobs), shared).with_submit(ctx);
    (admission, Writes::over(api, token()).await)
}

/// A policy of `max_attempts` with the waits `initial` and `max`, in seconds, as given.
fn waits(max_attempts: u32, initial: Option<u32>, max: Option<u32>) -> v1::RetryPolicy {
    v1::RetryPolicy {
        max_attempts,
        initial_backoff_seconds: initial,
        max_backoff_seconds: max,
    }
}

/// The 400 body of a field refused without a code.
fn invalid_body(field: &str) -> Value {
    json!({
        "error": "invalid-argument",
        "field": field,
        "reason": "the service refused an argument of the request",
    })
}

/// Asserts that, with retry accepted, a longest wait below the effective initial wait is
/// refused `INVALID_ARGUMENT` `field: spec.retry.maxBackoffSeconds` over gRPC, with no
/// `Refusal` detail, and over JSON as a 400 naming that field, before admission is asked and
/// with nothing recorded: below an initial wait given (5 and 4, 1 and 0, 3600 and 3599),
/// below the default of 10 seconds when none is given (9), and below the initial wait cut to
/// an hour (7200 given, 3599 refused). Catches a cap below its first wait taken in (a job
/// whose every wait would exceed its cap), a cap compared with the initial wait as given
/// instead of as cut, a default initial wait not applied, and a refusal that comes after
/// admission.
#[tokio::test]
async fn submit_refuses_a_backoff_cap_below_its_initial_wait() {
    let (admission, writes) = accepting().await;
    let cases = [
        (Some(5), 4),
        (Some(1), 0),
        (Some(3600), 3599),
        (None, 9),
        (None, 0),
        (Some(7200), 3599),
    ];
    for (index, (initial, max)) in cases.into_iter().enumerate() {
        let spec = v1::JobSpec {
            retry: Some(waits(3, initial, Some(max))),
            ..wire(&format!("cap-{index}"))
        };
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{index}");
        assert_eq!(
            status.message(),
            "field: spec.retry.maxBackoffSeconds",
            "{index}"
        );
        assert_eq!(refusal_of(&status), None, "{index}");
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body),
            (
                StatusCode::BAD_REQUEST,
                invalid_body("spec.retry.maxBackoffSeconds")
            ),
            "{index}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
    assert_eq!(admission.asked(), 0);
}

/// Asserts that, with retry accepted, a policy at the boundary is taken over both transports:
/// a longest wait equal to the initial wait given (5 and 5), equal to the default initial
/// wait (10 with none given), equal to the cut initial wait (3600 with 7200 given), absent
/// beside an initial wait or beside none, and 0, 1 and `u32::MAX` attempts. The answer
/// shows the policy as sent, and the stored record holds it as a `RetryPolicy` with the waits
/// in seconds and absent waits absent. Catches a boundary refused, a policy dropped or
/// changed on its way to the record (the cut initial wait stored as cut), and an absent wait
/// recorded as 0.
#[tokio::test]
async fn submit_takes_a_policy_at_the_boundary_and_keeps_it_as_sent() {
    let (_, writes) = accepting().await;
    let seconds = |wait: Option<u32>| wait.map(|s| Duration::from_secs(u64::from(s)));
    let cases = [
        waits(3, Some(5), Some(5)),
        waits(3, None, Some(10)),
        waits(3, Some(7200), Some(3600)),
        waits(3, Some(5), None),
        waits(3, None, None),
        waits(0, None, None),
        waits(1, Some(5), Some(600)),
        waits(u32::MAX, Some(5), Some(600)),
    ];
    let count = cases.len();
    for (index, policy) in cases.into_iter().enumerate() {
        for (n, via) in Via::BOTH.into_iter().enumerate() {
            let name = format!("ok-{index}-{n}");
            let spec = v1::JobSpec {
                retry: Some(policy),
                ..wire(&name)
            };
            let job = writes.submit(via, &spec).await.expect("the job is taken");
            let echoed = job.spec.expect("a spec").retry;
            assert_eq!(echoed, Some(policy), "{name}");
            let kept = writes.jobs.get_job(&id(&name)).unwrap().unwrap();
            assert_eq!(
                kept.spec.retry,
                Some(RetryPolicy {
                    max_attempts: policy.max_attempts,
                    initial_backoff: seconds(policy.initial_backoff_seconds),
                    max_backoff: seconds(policy.max_backoff_seconds),
                }),
                "{name}"
            );
        }
    }
    assert_eq!(rows(&writes.jobs), count * 2);
}

/// Asserts that, with retry accepted, a callback is still refused `spec.callback:
/// callback-not-enabled` over both transports, alone and beside a valid policy, with nothing
/// recorded. Catches the gate for retry opening the gate for the callback.
#[tokio::test]
async fn a_callback_is_still_refused_when_retry_is_accepted() {
    let (_, writes) = accepting().await;
    let called = ("spec.callback", "callback-not-enabled");
    let cases = [(None, called), (Some(waits(3, Some(5), Some(600))), called)];
    for (index, (retry, (field, code))) in cases.into_iter().enumerate() {
        let spec = v1::JobSpec {
            retry,
            callback: Some(callback()),
            ..wire(&format!("call-{index}"))
        };
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{index}");
        assert_eq!(
            status.message(),
            format!("field: {field}: {code}"),
            "{index}"
        );
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body),
            (StatusCode::BAD_REQUEST, refused_body(field, code)),
            "{index}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}
