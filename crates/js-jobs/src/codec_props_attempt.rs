//! The strategies of a job's retry policy and attempts, for the codec's properties.

use super::*;

/// A retry policy with each wait given or left to its default.
pub(super) fn retry_policy() -> impl Strategy<Value = js_types::RetryPolicy> {
    let wait = || {
        proptest::option::of((0..4000u64, 0..NANOS_PER_SEC).prop_map(|(s, n)| Duration::new(s, n)))
    };
    (any::<u32>(), wait(), wait()).prop_map(|(max_attempts, initial_backoff, max_backoff)| {
        js_types::RetryPolicy {
            max_attempts,
            initial_backoff,
            max_backoff,
        }
    })
}

/// A job's attempts: a number, ended attempts, a not-before time and hosts.
pub(super) fn attempt_trail() -> impl Strategy<Value = crate::AttemptTrail> {
    use crate::{Attempt, AttemptOutcome as O, AttemptRecord};
    let outcome = prop::sample::select(vec![
        O::Succeeded,
        O::Failed,
        O::HostLost,
        O::StartFailed,
        O::Cancelled,
    ]);
    let attempt = (
        1..=u32::MAX,
        host_id(),
        store_time(),
        outcome,
        proptest::option::of(any::<i32>()),
        any::<bool>(),
    )
        .prop_map(|(number, host, at, outcome, exit_code, retried)| Attempt {
            number,
            host,
            queued_at: at,
            placed_at: at,
            started_at: Some(at),
            ended_at: at,
            retried,
            carried: Carried::default(),
            end: AttemptRecord {
                outcome,
                exit_code,
                reason: Some(ResultReason::Exit),
                start_failure: None,
                start_failure_detail: None,
            },
        });
    (
        1..=u32::MAX,
        vec(attempt, 0..3),
        any::<u32>(),
        proptest::option::of(store_time()),
        proptest::collection::btree_set(host_id(), 0..4),
    )
        .prop_map(|(n, history, dropped, not_before, hosts)| {
            crate::AttemptTrail::from_parts(n, history, dropped, not_before, hosts)
        })
}
