//! `SubmitJob` of a job that carries a retry policy (`spec.retry`) or a completion callback
//! (`spec.callback`), over both transports through the mounted handler: this release acts on
//! neither, so it refuses both, before it looks the job's id up and before admission, and
//! records nothing (`docs/design/job-retry-and-completion-callback.md`, section 7).

use super::*;

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
