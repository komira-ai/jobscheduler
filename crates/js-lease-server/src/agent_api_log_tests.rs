//! Live logs through the heartbeat route (`docs/design/live-logs.md`, sections 3.1, 4 and 5): the
//! sink of each start, one credential per host minted off the request path and delivered in
//! replies, and what a failed mint costs. Renewal, `stale`, restarts, departures and the stored
//! lengths are in `agent_api_log_life_tests.rs`. The minter is js-logmint's fake over
//! js-logstore's fake store; time is the rig's manual clock; nothing sleeps.

use std::time::Duration;

use js_agent_wire::{HeartbeatReply, JobStartSpec, LogSink, Outcome};
use js_jobs::JobState;
use js_logmint::{MintError, Refusal};
use js_logstore::{Metadata, StoreError, Unavailable};
use serde_json::Value;

use super::job_rig::{
    FakeAgent, LOG_BUCKET, LOG_ENDPOINT, LOG_REGION, Rig, logging_rig, rig, starts,
};
use crate::api::whole_view_tests::Lines;

/// A grace no test outlives, so that a host is never lost while a test moves the clock.
pub(crate) const LONG_GRACE: u64 = js_agent_wire::limits::MAX_GRACE_MS;

/// The rig's tick: what every tick does for the agent side, the host credentials last.
pub(crate) async fn tick(rig: &Rig) {
    rig.agents.refresh().await;
}

/// The sink a start of the job `job` carries.
pub(crate) fn sink_of(job: &str) -> LogSink {
    LogSink {
        endpoint: LOG_ENDPOINT.to_owned(),
        region: LOG_REGION.to_owned(),
        bucket: LOG_BUCKET.to_owned(),
        prefix: format!("a/b/{job}/"),
        path_style: true,
    }
}

/// The start of `job` in `reply`.
pub(crate) fn start_of(reply: &HeartbeatReply, job: &str) -> JobStartSpec {
    let start = starts(reply).into_iter().find(|start| start.job_id == job);
    start.expect("the reply starts the job")
}

/// `reply` as JSON with every start's allocation id, which each placement draws at random,
/// set to one value, so that two servers' replies compare byte for byte.
pub(crate) fn normalised(reply: &HeartbeatReply) -> String {
    let mut value = serde_json::to_value(reply).unwrap();
    let commands = value["commands"].as_array_mut().into_iter().flatten();
    let ids = commands.filter_map(|command| command.get_mut("allocation_id"));
    ids.for_each(|id| *id = Value::from("allocation"));
    value.to_string()
}

/// Log lines go to `lines`, from this thread and from the mints it starts, until the guard
/// drops.
pub(crate) fn capture(lines: &Lines) -> tracing::subscriber::DefaultGuard {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_default(subscriber)
}

/// A placed job's start holds the configured store, region, bucket and style and the job's
/// own prefix under the log prefix, and no credential; the host's reply carries the
/// credential, which may put and delete under that job's prefix and under another job's below
/// the same log prefix, and nothing else: not under `a/bc/` beside the log prefix `a/b`, and no
/// get or list anywhere. Catches a sink with the wrong prefix, a secret in a start, and a
/// credential broader than the log prefix.
#[tokio::test]
async fn a_placed_job_starts_with_a_sink_naming_its_own_prefix_and_no_credential() {
    let (mut rig, logs) = logging_rig("logs_sink_prefix", LONG_GRACE);
    rig.join("host-a", true).await;
    logs.settle().await;
    let agent = FakeAgent::uploading("host-a");
    rig.place("job-a", "host-a");
    let reply = rig.beat(&agent).await.unwrap();
    let start = start_of(&reply, "job-a");
    assert_eq!(start.log_sink, Some(sink_of("job-a")));
    let grant = reply
        .log_grant
        .clone()
        .expect("the host's reply carries its credential");
    let token = grant.session_token.clone().unwrap();
    let text = serde_json::to_string(&start).unwrap();
    for secret in [&grant.access_key_id, &grant.secret_access_key, &token] {
        assert!(
            !text.contains(secret.as_str()),
            "the start carries a secret: {text}"
        );
    }

    let store = logs.client(&grant);
    let metadata = Metadata::default();
    let ours = "a/b/job-a/stdout/00000000000000000000";
    let theirs = "a/b/job-z/stderr/00000000000000000000";
    for key in [ours, theirs] {
        store.put(key, b"bytes".to_vec(), &metadata).unwrap();
        store.delete(key).unwrap();
    }
    let denied = |answer: Result<(), StoreError>, what: &str| {
        assert!(
            matches!(answer, Err(StoreError::Denied { .. })),
            "{what}: {answer:?}"
        );
    };
    let outside = "a/bc/job-a/stdout/00000000000000000000";
    denied(
        store.put(outside, b"bytes".to_vec(), &metadata),
        "a put outside",
    );
    denied(store.delete(outside), "a delete outside");
    store.put(ours, b"bytes".to_vec(), &metadata).unwrap();
    for key in [ours, outside] {
        denied(store.get_range(key, 0, 5).map(drop), "a get");
    }
    for prefix in ["a/b/", "a/b/job-a/", "a/bc/", ""] {
        denied(store.list(prefix, None, 10).map(drop), "a list");
    }
}

/// A host whose hello did not report `log-upload` gets starts with no sink and no grant from
/// a server with a sink, and a host that reported it gets neither from a server with
/// `sink = "none"`: both replies are byte for byte the reply of a server without live logs, and
/// the minter is never called. Catches a credential minted or sent, or a sink sent, where it
/// cannot be used.
#[tokio::test]
async fn no_sink_for_a_host_that_did_not_report_the_capability_or_with_sink_none() {
    let mut before = rig("logs_none_before", true);
    before.register("host-a", true);
    before.place("job-a", "host-a");
    let today = before.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert!(starts(&today)[0].log_sink.is_none());

    let (mut sinked, logs) = logging_rig("logs_none_incapable", LONG_GRACE);
    sinked.join("host-a", false).await;
    sinked.place("job-a", "host-a");
    let mut replies = Vec::new();
    for agent in [FakeAgent::new("host-a"), FakeAgent::uploading("host-a")] {
        replies.push(sinked.beat(&agent).await.unwrap());
    }
    tick(&sinked).await;
    logs.settle().await;

    let mut none = rig("logs_none_sink_none", true);
    none.join("host-a", true).await;
    none.place("job-a", "host-a");
    replies.push(none.beat(&FakeAgent::uploading("host-a")).await.unwrap());
    for reply in &replies {
        assert_eq!(normalised(reply), normalised(&today));
        let text = serde_json::to_string(reply).unwrap();
        assert!(
            !text.contains("log_sink") && !text.contains("log_grant"),
            "{text}"
        );
    }
    assert_eq!(logs.minter.calls("host-a"), 0);
    assert!(logs.minter.requests().is_empty());
}

/// Three jobs on one host and one on another: one mint per host, every start of a host
/// carries its own job's sink and no credential, and the host's one grant serves all its
/// jobs; once the agent holds it, the next reply carries none and nothing is minted again.
/// Catches a mint per job.
#[tokio::test]
async fn one_credential_serves_every_job_on_a_host() {
    let (mut rig, logs) = logging_rig("logs_one_per_host", LONG_GRACE);
    rig.join("host-a", true).await;
    rig.join("host-b", true).await;
    logs.settle().await;
    for job in ["job-1", "job-2", "job-3"] {
        rig.place(job, "host-a");
    }
    rig.place("job-4", "host-b");
    let mut a = FakeAgent::uploading("host-a");
    let mut b = FakeAgent::uploading("host-b");
    let reply_a = rig.beat(&a).await.unwrap();
    let reply_b = rig.beat(&b).await.unwrap();
    for (reply, jobs) in [
        (&reply_a, &["job-1", "job-2", "job-3"][..]),
        (&reply_b, &["job-4"][..]),
    ] {
        assert_eq!(starts(reply).len(), jobs.len());
        for job in jobs {
            assert_eq!(start_of(reply, job).log_sink, Some(sink_of(job)));
        }
    }
    let grant_a = reply_a.log_grant.clone().unwrap();
    let grant_b = reply_b.log_grant.clone().unwrap();
    assert_ne!(grant_a.access_key_id, grant_b.access_key_id);
    assert_eq!(logs.grants.credential_for("host-a"), Some(grant_a.clone()));
    a.take(&reply_a);
    b.take(&reply_b);
    for _ in 0..3 {
        assert_eq!(rig.beat(&a).await.unwrap().log_grant, None);
        assert_eq!(rig.beat(&b).await.unwrap().log_grant, None);
    }
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(
        (logs.minter.calls("host-a"), logs.minter.calls("host-b")),
        (1, 1)
    );
    assert_eq!(a.grant, Some(grant_a));
}

/// With the minter holding every mint, a placed job's first reply carries its start with
/// the sink at once and no grant; once the mint is let go, the next reply carries the grant.
/// With the minter failing, the start is the same. Catches a start held back for a mint.
#[tokio::test]
async fn a_start_never_waits_for_a_mint() {
    let (mut rig, logs) = logging_rig("logs_start_no_wait", LONG_GRACE);
    logs.minter.block();
    rig.join("host-a", true).await;
    logs.held(1).await;
    rig.place("job-a", "host-a");
    let agent = FakeAgent::uploading("host-a");
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(start_of(&reply, "job-a").log_sink, Some(sink_of("job-a")));
    assert_eq!(reply.log_grant, None);
    logs.minter.release();
    logs.settle().await;
    let reply = rig.beat(&agent).await.unwrap();
    assert!(reply.log_grant.is_some());
    assert_eq!(start_of(&reply, "job-a").log_sink, Some(sink_of("job-a")));

    let unavailable = Unavailable::Status {
        status: 503,
        code: "SlowDown".to_owned(),
    };
    logs.minter
        .fail_next(1, MintError::Unavailable(unavailable));
    rig.join("host-b", true).await;
    logs.settle().await;
    rig.place("job-b", "host-b");
    let reply = rig.beat(&FakeAgent::uploading("host-b")).await.unwrap();
    assert_eq!(start_of(&reply, "job-b").log_sink, Some(sink_of("job-b")));
    assert_eq!(reply.log_grant, None);
    assert_eq!(logs.minter.calls("host-b"), 1);
}

/// A job whose id the object layout builds no prefix for (it begins with `_` or `-`) has no
/// sink: the wire's own id rule already keeps its start from being sent, and the reply is
/// the reply of a server without live logs, while a job beside it with a good id gets its
/// sink. Catches a prefix built from an id the layout refuses.
#[tokio::test]
async fn a_job_whose_id_cannot_make_a_prefix_has_no_sink() {
    let (mut rig, logs) = logging_rig("logs_bad_id", LONG_GRACE);
    let mut before = super::job_rig::rig("logs_bad_id_before", true);
    for bad in ["_lead", "-lead"] {
        assert_eq!(logs.grants.sink_for(bad), None, "{bad}");
    }
    assert_eq!(logs.grants.sink_for("ok-1"), Some(sink_of("ok-1")));
    rig.join("host-a", true).await;
    before.register("host-a", true);
    for job in ["_lead", "-lead"] {
        rig.place(job, "host-a");
        before.place(job, "host-a");
    }
    let reply = rig.beat(&FakeAgent::uploading("host-a")).await.unwrap();
    let today = before.beat(&FakeAgent::new("host-a")).await.unwrap();
    let mut without_grant = reply.clone();
    without_grant.log_grant = None;
    assert_eq!(normalised(&without_grant), normalised(&today));
    assert!(starts(&reply).is_empty());
    rig.place("ok-1", "host-a");
    let reply = rig.beat(&FakeAgent::uploading("host-a")).await.unwrap();
    assert_eq!(starts(&reply).len(), 1);
    assert_eq!(start_of(&reply, "ok-1").log_sink, Some(sink_of("ok-1")));
}

/// A mint that fails at registration, `Unavailable` or `Refused`, costs the host its live
/// upload and nothing else: its job is placed, started, run and ended, and its result stored,
/// as always. The failure is logged once for the episode, with no secret; the mint is retried
/// on later ticks after 5 s, then 10 s, then 20 s of the clock (never while the clock has not
/// moved, never twice in an interval, never from a heartbeat), and the success gives a grant
/// in the next reply. Catches a mint failure that changes a job's state, a retry without bound
/// or in a tight loop, and a mint never retried.
#[tokio::test]
async fn a_mint_failure_costs_the_host_its_live_upload_until_a_later_mint_succeeds_and_nothing_else()
 {
    let unavailable = MintError::Unavailable(Unavailable::Status {
        status: 503,
        code: "SlowDown".to_owned(),
    });
    let refused = MintError::Refused(Refusal::Status {
        status: 403,
        code: "AccessDenied".to_owned(),
    });
    for (n, error) in [unavailable, refused].into_iter().enumerate() {
        let lines = Lines::default();
        let _capture = capture(&lines);
        let (mut rig, logs) = logging_rig(&format!("logs_mint_fails_{n}"), LONG_GRACE);
        logs.minter.fail_next(3, error.clone());
        rig.join("host-a", true).await;
        logs.settle().await;
        let calls = || logs.minter.calls("host-a");
        assert_eq!(calls(), 1);

        let id = rig.place("job-a", "host-a");
        let mut agent = FakeAgent::uploading("host-a");
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(start_of(&reply, "job-a").log_sink, Some(sink_of("job-a")));
        assert_eq!(reply.log_grant, None);
        agent.take(&reply);
        agent.run("job-a");
        rig.beat(&agent).await.unwrap();
        agent.end("job-a", Outcome::Succeeded, Some(0));
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(reply.job_acks.len(), 1, "{error:?}");
        assert_eq!(rig.job(&id).state, JobState::Succeeded);
        assert!(rig.job(&id).result.is_some());

        // Each wait is waited out exactly: a millisecond before it, ticks and beats mint
        // nothing; at it, the next tick makes one attempt.
        for (attempt, wait) in [(2, 5_000), (3, 10_000), (4, 20_000)] {
            for _ in 0..3 {
                tick(&rig).await;
                rig.beat(&agent).await.unwrap();
                logs.settle().await;
            }
            assert_eq!(calls(), attempt - 1, "{error:?}: tried before the wait");
            rig.advance(Duration::from_millis(wait - 1));
            tick(&rig).await;
            logs.settle().await;
            assert_eq!(calls(), attempt - 1, "{error:?}: tried a millisecond early");
            rig.advance(Duration::from_millis(1));
            tick(&rig).await;
            logs.settle().await;
            assert_eq!(calls(), attempt, "{error:?}: not tried at the wait");
        }
        let reply = rig.beat(&agent).await.unwrap();
        let grant = reply
            .log_grant
            .expect("the mint that succeeded is delivered");
        assert_eq!(logs.grants.counters().failed, 3);
        assert_eq!(logs.grants.counters().minted, 1);

        let text = lines.take();
        let warned = "cannot mint a host's log credential";
        assert_eq!(text.matches(warned).count(), 1, "{text}");
        assert_eq!(
            text.matches("minted after failed mints").count(),
            1,
            "{text}"
        );
        let token = grant.session_token.unwrap_or_default();
        for secret in [grant.access_key_id, grant.secret_access_key, token] {
            assert!(!text.contains(&secret), "{text}");
        }
    }
}
