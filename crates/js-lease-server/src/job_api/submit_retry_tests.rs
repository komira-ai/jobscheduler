//! `SubmitJob` of a job that carries a retry policy (`spec.retry`) or a completion callback
//! (`spec.callback`), over both transports through the mounted handler, as a server serves it:
//! both are taken, a bad one is refused by its field before its id is looked up and before
//! admission, and both are part of the job a same-id submit is compared on
//! (`docs/design/job-retry-and-completion-callback.md`, sections 3.1, 5.2 and 7). The callback's
//! own rules and its delivery are in `callback_tests.rs`.

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

/// The service over a fake store whose submit is served as a server serves it, a callback's
/// address judged by the rule that knows no cluster address, admitting every job; the source
/// of the admission with it.
async fn served() -> (Arc<Fixed>, Writes) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let admission = Fixed::new(true);
    let ctx = Ctx::served(CEILING, admission.clone());
    let shared = SharedClock::new(OneClock(clock));
    let api = JobApi::new(Arc::new(jobs), shared).with_submit(ctx);
    (admission, Writes::over(api, token()).await)
}

/// `callback` as every answer shows it: its URL and deadline, and no key.
fn shown(callback: &v1::Callback) -> v1::Callback {
    v1::Callback {
        signing_key: Vec::new(),
        ..callback.clone()
    }
}

/// Asserts that a served submit takes a job carrying a retry policy and a completion callback,
/// together and each alone, and an empty policy, over both transports: the answer shows the
/// policy as sent and the callback with its key empty, and the record holds both. Asserts that
/// a bad one is refused `INVALID_ARGUMENT` with no `Refusal` detail over gRPC, and over JSON
/// with the whole 400 body naming the field and nothing else, before admission and with
/// nothing recorded: a plain-http URL, an empty callback (whose URL is the first fault), a
/// literal loopback address and the metadata address the address rule denies, a key one byte
/// short and one byte long, a backoff cap below its initial wait, and a bad policy beside a bad
/// callback, which names the policy. Catches a refusal left in place for either field, a
/// policy or a callback dropped on its way to the record, a key shown in an answer, a bad
/// callback or policy taken, a refusal that repeats the URL or the key, and a refusal after
/// admission.
#[tokio::test]
async fn submit_takes_retry_and_callback_and_refuses_bad_ones() {
    let (admission, writes) = served().await;
    let taken = [
        (Some(policy(3)), Some(callback())),
        (Some(policy(2)), None),
        (None, Some(callback())),
        (Some(v1::RetryPolicy::default()), None),
    ];
    for (index, (retry, called)) in taken.iter().enumerate() {
        for via in Via::BOTH {
            let name = format!("takes-{index}-{via:?}");
            let spec = v1::JobSpec {
                retry: *retry,
                callback: called.clone(),
                ..wire(&name)
            };
            let job = writes.submit(via, &spec).await.expect("the job is taken");
            let echoed = job.spec.expect("a spec");
            assert_eq!(echoed.retry, *retry, "{name}");
            assert_eq!(echoed.callback, called.as_ref().map(shown), "{name}");
            let kept = writes.jobs.get_job(&id(&name)).unwrap().unwrap();
            assert_eq!(kept.spec.retry.is_some(), retry.is_some(), "{name}");
            let kept_url = kept.spec.callback.as_ref().map(|c| c.url.clone());
            assert_eq!(kept_url, called.as_ref().map(|c| c.url.clone()), "{name}");
        }
    }
    let admitted = admission.asked();
    assert_eq!(admitted, taken.len() * 2);
    let with_url = |url: &str| v1::Callback {
        url: url.to_owned(),
        ..callback()
    };
    let with_key = |len: usize| v1::Callback {
        signing_key: vec![7; len],
        ..callback()
    };
    let url_field = |code: &str| format!("spec.callback.url: {code}");
    let refused = [
        (
            None,
            Some(with_url("http://hooks.example/js-done/secret-ref")),
            url_field("not-https"),
        ),
        (None, Some(v1::Callback::default()), url_field("not-https")),
        (
            None,
            Some(with_url("https://127.0.0.1/js-done/secret-ref")),
            url_field("refused-address"),
        ),
        (
            None,
            Some(with_url("https://169.254.169.254/js-done/secret-ref")),
            url_field("refused-address"),
        ),
        (
            None,
            Some(with_key(31)),
            "spec.callback.signingKey: bad-length".to_owned(),
        ),
        (
            None,
            Some(with_key(65)),
            "spec.callback.signingKey: bad-length".to_owned(),
        ),
        (
            Some(waits(3, Some(5), Some(4))),
            None,
            "spec.retry.maxBackoffSeconds".to_owned(),
        ),
        (
            Some(waits(3, Some(5), Some(4))),
            Some(with_key(31)),
            "spec.retry.maxBackoffSeconds".to_owned(),
        ),
    ];
    for (index, (retry, called, field)) in refused.into_iter().enumerate() {
        let spec = v1::JobSpec {
            retry,
            callback: called,
            ..wire(&format!("bad-{index}"))
        };
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{index}");
        assert_eq!(status.message(), format!("field: {field}"), "{index}");
        assert_eq!(refusal_of(&status), None, "{index}");
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body),
            (StatusCode::BAD_REQUEST, invalid_body(&field)),
            "{index}"
        );
    }
    assert_eq!(rows(&writes.jobs), taken.len() * 2);
    assert_eq!(admission.asked(), admitted);
}

/// Asserts that the retry policy and the callback are part of the job a same-id submit is
/// compared on: a job recorded without either and submitted again with a policy, or with a
/// callback, is `job-id-conflict` (`ALREADY_EXISTS` with the refusal, 409) over both
/// transports; one recorded with both is answered with itself when submitted again with the
/// same ones, and is `job-id-conflict` with another `maxAttempts`, another URL or another key
/// (which no answer shows); a bad policy or a bad key on that id is refused by its field
/// (`INVALID_ARGUMENT`, 400) before the id is looked up, not answered as a conflict; and the
/// recorded jobs are unchanged. Catches a resubmit answered
/// from the stored job while it asks for another policy or callback, so its client would read
/// the one it asked for as dropped, and a key left out of the comparison.
#[tokio::test]
async fn a_resubmit_with_another_policy_or_callback_is_a_job_id_conflict() {
    let (_, writes) = served().await;
    let plain = writes.submit_grpc(&wire("held-1")).await.unwrap();
    let both = v1::JobSpec {
        retry: Some(policy(2)),
        callback: Some(callback()),
        ..wire("held-2")
    };
    let recorded = writes.submit_grpc(&both).await.unwrap();
    let other_url = v1::Callback {
        url: "https://hooks.example/js-done/other-ref".to_owned(),
        ..callback()
    };
    let other_key = v1::Callback {
        signing_key: vec![9; 32],
        ..callback()
    };
    let conflicts = [
        v1::JobSpec {
            retry: Some(policy(2)),
            ..wire("held-1")
        },
        v1::JobSpec {
            callback: Some(callback()),
            ..wire("held-1")
        },
        v1::JobSpec {
            retry: Some(policy(3)),
            ..both.clone()
        },
        v1::JobSpec {
            callback: Some(other_url),
            ..both.clone()
        },
        v1::JobSpec {
            callback: Some(other_key),
            ..both.clone()
        },
    ];
    for (index, spec) in conflicts.iter().enumerate() {
        let status = writes.submit_grpc(spec).await.unwrap_err();
        assert_eq!(status.code(), Code::AlreadyExists, "{index}");
        let carried = refusal_of(&status).map(|refusal| refusal.code());
        assert_eq!(carried, Some(v1::RefusalCode::JobIdConflict), "{index}");
        let (http, body) = writes.submit(Via::Json, spec).await.unwrap_err();
        assert_eq!(
            (http, body),
            (StatusCode::CONFLICT, json!({ "error": "job-id-conflict" })),
            "{index}"
        );
    }
    let bad = [
        (
            v1::JobSpec {
                retry: Some(waits(3, Some(5), Some(4))),
                ..both.clone()
            },
            "spec.retry.maxBackoffSeconds",
        ),
        (
            v1::JobSpec {
                callback: Some(v1::Callback {
                    signing_key: vec![7; 31],
                    ..callback()
                }),
                ..both.clone()
            },
            "spec.callback.signingKey: bad-length",
        ),
    ];
    for (spec, field) in &bad {
        let status = writes.submit_grpc(spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{field}");
        assert_eq!(status.message(), format!("field: {field}"), "{field}");
        let (http, body) = writes.submit(Via::Json, spec).await.unwrap_err();
        let refused = (StatusCode::BAD_REQUEST, invalid_body(field));
        assert_eq!((http, body), refused, "{field}");
    }
    for via in Via::BOTH {
        let again = writes.submit(via, &both).await.expect("the same job");
        assert_eq!(again, recorded, "{via:?}");
    }
    assert_eq!(rows(&writes.jobs), 2);
    assert_eq!(writes.submit_grpc(&wire("held-1")).await.unwrap(), plain);
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

/// Asserts that a longest wait below the effective initial wait is
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
    let (admission, writes) = served().await;
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

/// Asserts that a policy at the boundary is taken over both transports:
/// a longest wait equal to the initial wait given (5 and 5), equal to the default initial
/// wait (10 with none given), equal to the cut initial wait (3600 with 7200 given), absent
/// beside an initial wait or beside none, and 0, 1 and `u32::MAX` attempts. The answer
/// shows the policy as sent, and the stored record holds it as a `RetryPolicy` with the waits
/// in seconds and absent waits absent. Catches a boundary refused, a policy dropped or
/// changed on its way to the record (the cut initial wait stored as cut), and an absent wait
/// recorded as 0.
#[tokio::test]
async fn submit_takes_a_policy_at_the_boundary_and_keeps_it_as_sent() {
    let (_, writes) = served().await;
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
