//! `dropped_policies`: what a submit's answer says the server dropped, read from the answers a
//! server that knows `retry` and `callback` gives and from the answer of one released before
//! them, which skips both fields.

use std::time::Duration;

use js_client::jobs::{DroppedPolicy, JobCalls, JobClient, dropped_policies};
use js_proto::v1::{Callback, Job, JobSpec, RetryPolicy, SubmitJobRequest, SubmitJobResponse};
use prost::Message as _;

use crate::fake_jobs::{Answer, Fake};
use crate::support::Scratch;

/// A retry policy of three attempts.
fn policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        initial_backoff_seconds: Some(5),
        max_backoff_seconds: Some(60),
    }
}

/// A callback with a 32-byte key.
fn callback() -> Callback {
    Callback {
        url: "https://hooks.example/js-done/ref".to_owned(),
        signing_key: vec![7; 32],
        deadline_seconds: None,
    }
}

/// A submit of job `j-1` carrying `retry` and `callback`.
fn sent(retry: Option<RetryPolicy>, callback: Option<Callback>) -> SubmitJobRequest {
    SubmitJobRequest {
        spec: Some(JobSpec {
            job_id: "j-1".to_owned(),
            image: "registry.example/app@sha256:0".to_owned(),
            retry,
            callback,
            ..JobSpec::default()
        }),
    }
}

/// The answer of a server that recorded job `j-1` with the spec `echoed`.
fn answered(echoed: Option<JobSpec>) -> SubmitJobResponse {
    SubmitJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            spec: echoed,
            ..Job::default()
        }),
    }
}

/// `request`'s spec as a server that knows both fields echoes it: the callback's key empty.
fn echoed_by_a_current_server(request: &SubmitJobRequest) -> JobSpec {
    let mut spec = request.spec.clone().unwrap_or_default();
    if let Some(callback) = spec.callback.as_mut() {
        callback.signing_key.clear();
    }
    spec
}

/// `request`'s spec as a server released before both fields reads it: decoded by a message
/// that has neither, so both are skipped.
fn echoed_by_an_older_server(request: &SubmitJobRequest) -> JobSpec {
    JobSpec {
        retry: None,
        callback: None,
        ..request.spec.clone().unwrap_or_default()
    }
}

/// Asserts that a submit whose answer leaves out a retry policy or a callback the request set
/// is reported as dropped, the retry policy first, and that nothing is reported where the
/// answer carries what was sent: an older server's answer to a job with both reports both,
/// with one each reports that one, and a current server's answer (the callback's key empty)
/// reports nothing. Asserts the boundary cases: a request with neither field, or with no spec,
/// reports nothing whatever the answer; an answer with no job, or a job with no spec, reports
/// every policy sent; and a server that kept one and dropped the other reports only the one
/// dropped. Asserts each policy's field name. Catches a report that judges the callback by its
/// key (always empty in an answer), one that reports a policy the request never set, and one
/// that reads a missing answer as a kept policy.
#[test]
fn a_dropped_policy_is_reported() {
    use DroppedPolicy::{Callback as Called, Retry};
    let cases = [
        (sent(Some(policy()), Some(callback())), vec![Retry, Called]),
        (sent(Some(policy()), None), vec![Retry]),
        (sent(None, Some(callback())), vec![Called]),
        (sent(None, None), vec![]),
    ];
    for (index, (request, by_an_older_one)) in cases.into_iter().enumerate() {
        let older = answered(Some(echoed_by_an_older_server(&request)));
        assert_eq!(
            dropped_policies(&request, &older),
            by_an_older_one,
            "{index}"
        );
        let current = answered(Some(echoed_by_a_current_server(&request)));
        assert_eq!(dropped_policies(&request, &current), vec![], "{index}");
        for empty in [SubmitJobResponse { job: None }, answered(None)] {
            assert_eq!(
                dropped_policies(&request, &empty),
                by_an_older_one,
                "{index}"
            );
        }
    }
    let no_spec = SubmitJobRequest { spec: None };
    let full = answered(Some(echoed_by_a_current_server(&sent(
        Some(policy()),
        Some(callback()),
    ))));
    assert_eq!(dropped_policies(&no_spec, &full), vec![]);
    assert_eq!(
        dropped_policies(&no_spec, &SubmitJobResponse { job: None }),
        vec![]
    );
    let both = sent(Some(policy()), Some(callback()));
    let kept_retry = JobSpec {
        callback: None,
        ..echoed_by_a_current_server(&both)
    };
    assert_eq!(
        dropped_policies(&both, &answered(Some(kept_retry))),
        vec![Called]
    );
    let kept_callback = JobSpec {
        retry: None,
        ..echoed_by_a_current_server(&both)
    };
    assert_eq!(
        dropped_policies(&both, &answered(Some(kept_callback))),
        vec![Retry]
    );
    assert_eq!((Retry.field(), Called.field()), ("retry", "callback"));
}

/// Asserts that the answer a real `JobClient` decodes off the wire, from a server that echoes
/// the spec without `retry` and `callback` (the bytes an older server writes), is reported as
/// having dropped both. Catches a report that only works on answers built in memory.
#[tokio::test]
async fn an_older_servers_answer_on_the_wire_reports_both_dropped() {
    let dir = Scratch::new("dropped_wire");
    let token = dir.write("token", b"job-token-0123456789\n", 0o600);
    let request = sent(Some(policy()), Some(callback()));
    let older = answered(Some(echoed_by_an_older_server(&request)));
    let fake = Fake::new();
    fake.answer("SubmitJob", vec![Answer::Message(older.encode_to_vec())]);
    let mut client = JobClient::connect_via(
        "http://localhost:7070",
        &token,
        None,
        Duration::from_secs(20),
        fake.dialer(None),
    )
    .await
    .unwrap();
    let answer = JobCalls::submit_job(&mut client, request.clone())
        .await
        .unwrap();
    assert_eq!(
        dropped_policies(&request, &answer),
        vec![DroppedPolicy::Retry, DroppedPolicy::Callback]
    );
}
