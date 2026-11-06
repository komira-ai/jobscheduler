//! The life of a host credential through the heartbeat route (`docs/design/live-logs.md`,
//! sections 4 and 5): renewal and delivery until confirmed, `stale`, a host without the
//! capability, a heartbeat that never waits for the minter, a server restart, a host that
//! leaves, the stream lengths stored with a result, and no secret in a log line, the status
//! page or an error. Time is the rig's manual clock; nothing sleeps.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{HeldJob, HeldState, LogGrant, LogReport, NetworkCeiling, Outcome};
use js_jobs::{JobId, JobState, LogLengths, Retention};
use js_logmint::{MintError, Refusal};
use js_logstore::{Endpoint, Unavailable};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{ManualClock as StateClock, Store};
use js_testkit::dir::TestDir;
use js_types::HostId;

use super::job_rig::{
    BOOKED, FakeAgent, LOG_BUCKET, LOG_REGION, Rig, START_MS, grants_over, logging_over,
    logging_rig, placed, result, spec, starts,
};
use super::log_tests::{LONG_GRACE, capture, sink_of, start_of, tick};
use crate::adapt::SharedClock;
use crate::api::whole_view_tests::Lines;
use crate::job_store::ServerJobs;
use crate::per_job::{self, BeatContext};
use crate::status::{Status, page};

/// Half of [`LOG_TTL`], in milliseconds.
const HALF_LIFE_MS: u64 = 1_800_000;

/// The report of a job that printed 2.5 MiB to standard output and 100 bytes to standard error.
const REPORT: LogReport = LogReport {
    stdout_bytes: 2_621_440,
    stderr_bytes: 100,
    stdout_dropped: 0,
    stderr_dropped: 0,
};

/// [`REPORT`] as the job tables keep it.
const LENGTHS: LogLengths = LogLengths {
    stdout_bytes: 2_621_440,
    stderr_bytes: 100,
    stdout_dropped: 0,
    stderr_dropped: 0,
};

/// The credential's three texts, which may appear in no log line, page or error.
fn secrets(grant: &LogGrant) -> [String; 3] {
    [
        grant.access_key_id.clone(),
        grant.secret_access_key.clone(),
        grant.session_token.clone().unwrap_or_default(),
    ]
}

/// Past half of its life, the next tick mints the host's credential again (not a millisecond
/// before); every reply carries the new one while the heartbeat reports the old expiry (a
/// reply lost in between changes nothing), and once a heartbeat reports the new expiry it is
/// not sent again. Catches a late renewal, a renewal sent once only (a heartbeat with the old
/// expiry taken as confirmation), and one sent forever.
#[tokio::test]
async fn the_host_credential_is_renewed_before_half_its_life_is_gone_and_delivered_until_confirmed()
{
    let (mut rig, logs) = logging_rig("logs_renewed", LONG_GRACE);
    rig.join("host-a", true).await;
    logs.settle().await;
    let mut agent = FakeAgent::uploading("host-a");
    let reply = rig.beat(&agent).await.unwrap();
    agent.take(&reply);
    let first = agent.grant.clone().unwrap();
    assert_eq!(first.expires_at_ms, START_MS + 2 * HALF_LIFE_MS);

    rig.advance(Duration::from_millis(HALF_LIFE_MS));
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(
        logs.minter.calls("host-a"),
        1,
        "renewed with half its life left"
    );
    rig.advance(Duration::from_millis(1));
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(
        logs.minter.calls("host-a"),
        2,
        "not renewed past half its life"
    );

    let renewed = rig.beat(&agent).await.unwrap().log_grant.unwrap();
    assert!(renewed.expires_at_ms > first.expires_at_ms);
    assert_ne!(renewed.access_key_id, first.access_key_id);
    // The reply is lost: the agent still reports the old expiry, and is sent the new one.
    assert_eq!(
        agent.log_state().unwrap().until_ms,
        Some(first.expires_at_ms)
    );
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.log_grant, Some(renewed.clone()));
    agent.take(&reply);
    for _ in 0..3 {
        assert_eq!(rig.beat(&agent).await.unwrap().log_grant, None);
    }
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(logs.minter.calls("host-a"), 2);
}

/// A host whose hello lacks `log-upload` is sent no grant on any reply and causes no mint,
/// also after the minter recovers from a failure, while a host beside it that reported the
/// capability is sent one; a hello again with the capability keeps the credential held, and
/// one without it has the credential forgotten. Catches a credential minted or resent for a host that cannot use it.
#[tokio::test]
async fn a_host_without_the_capability_gets_no_grant() {
    let grace = js_agent_wire::DEFAULT_GRACE_MS;
    let (mut rig, logs) = logging_rig("logs_incapable", grace);
    let unavailable = Unavailable::Status {
        status: 500,
        code: "InternalError".to_owned(),
    };
    logs.minter
        .fail_next(1, MintError::Unavailable(unavailable));
    rig.join("host-a", false).await;
    rig.join("host-b", true).await;
    logs.settle().await;
    rig.advance(Duration::from_millis(5_000));
    tick(&rig).await;
    logs.settle().await;
    let incapable = FakeAgent::uploading("host-a");
    let capable = FakeAgent::uploading("host-b");
    for _ in 0..5 {
        assert_eq!(rig.beat(&incapable).await.unwrap().log_grant, None);
        tick(&rig).await;
        logs.settle().await;
    }
    assert_eq!(logs.minter.calls("host-a"), 0);
    assert_eq!(logs.grants.credential_for("host-a"), None);
    assert!(rig.beat(&capable).await.unwrap().log_grant.is_some());
    assert_eq!(logs.minter.calls("host-b"), 2);
    // A hello again (once the host fell silent, as an agent that restarted) with the
    // capability, while the host holds a credential, mints nothing.
    rig.advance(Duration::from_millis(grace + 1));
    rig.join("host-b", true).await;
    logs.settle().await;
    assert_eq!(logs.minter.calls("host-b"), 2);
    assert!(logs.grants.credential_for("host-b").is_some());

    rig.advance(Duration::from_millis(grace + 1));
    rig.join("host-b", false).await;
    assert_eq!(logs.grants.credential_for("host-b"), None);
    assert_eq!(rig.beat(&capable).await.unwrap().log_grant, None);
}

/// A heartbeat reporting `stale` while the agent holds the credential held here queues a mint
/// in that tick, and the reply after it carries the new credential, with no wait for the
/// half-life rule (the clock never moves, so the new credential expires at the same instant as
/// the refused one, as two mints within one second of an STS do); ten such heartbeats in one tick mint once, and a second `stale` in the same
/// tick mints nothing until the next tick. An agent reporting `stale` with no credential (it
/// restarted) is sent the one held. Catches a stale agent left waiting, and a scheduler made
/// to mint without bound.
#[tokio::test]
async fn a_stale_flag_makes_a_new_credential_at_once() {
    let (mut rig, logs) = logging_rig("logs_stale", LONG_GRACE);
    rig.join("host-a", true).await;
    logs.settle().await;
    let mut agent = FakeAgent::uploading("host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    let first = agent.grant.clone().unwrap();

    let restarted = FakeAgent::uploading("host-a");
    assert_eq!(
        rig.beat(&restarted).await.unwrap().log_grant,
        Some(first.clone())
    );
    assert_eq!(logs.minter.calls("host-a"), 1);

    agent.stale = true;
    logs.minter.block();
    for _ in 0..10 {
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(reply.log_grant, None);
    }
    logs.held(1).await;
    assert_eq!(logs.minter.calls("host-a"), 2);
    logs.minter.release();
    logs.settle().await;
    assert_eq!(logs.minter.calls("host-a"), 2);
    let reply = rig.beat(&agent).await.unwrap();
    let second = reply.log_grant.clone().unwrap();
    assert_ne!(second.access_key_id, first.access_key_id);
    agent.take(&reply);

    agent.stale = true;
    rig.beat(&agent).await.unwrap();
    logs.settle().await;
    assert_eq!(
        logs.minter.calls("host-a"),
        2,
        "a second stale in one tick minted"
    );
    tick(&rig).await;
    rig.beat(&agent).await.unwrap();
    logs.settle().await;
    assert_eq!(
        logs.minter.calls("host-a"),
        3,
        "the next tick's stale did not mint"
    );
}

/// With the minter held on its channel, heartbeats are answered, of a host whose mint is held
/// and of a host with none queued yet: the replies are built and returned while the minter
/// still holds the mint. Catches a reply path that waits on a mint, which would let a slow STS
/// make a host look silent.
#[tokio::test]
async fn a_heartbeat_never_calls_the_minter() {
    let (mut rig, logs) = logging_rig("logs_beat_no_mint", LONG_GRACE);
    logs.minter.block();
    rig.join("host-a", true).await;
    logs.held(1).await;
    rig.place("job-a", "host-a");
    let reply = rig.beat(&FakeAgent::uploading("host-a")).await.unwrap();
    assert_eq!(
        start_of(&reply, "job-a").log_sink,
        Some(sink_of("host-a", "job-a"))
    );
    assert_eq!(reply.log_grant, None);
    rig.register_uploading("host-b");
    let reply = rig.beat(&FakeAgent::uploading("host-b")).await.unwrap();
    assert_eq!(reply.log_grant, None);
    // Both were answered while their mints are held: host-b's beat queued one and went on.
    logs.held(2).await;
    assert_eq!(logs.minter.requests().len(), 2);
    logs.minter.release();
    logs.settle().await;
    assert!(logs.grants.credential_for("host-a").is_some());
    assert!(logs.grants.credential_for("host-b").is_some());
}

/// A server rebuilt on the same store holds no credential: each capable agent's next contact
/// mints again, a hello (after the old session is refused) and a first beat of a host whose
/// hello the server holds, and the credential reaches both, the one that reports an old
/// expiry and the one that reports `stale` with none. No stored row and no file of the
/// server holds an access key id, a secret or a session token. Catches a credential that
/// survives in the store, and a host left without one.
#[tokio::test]
async fn a_restart_mints_again_for_each_capable_host() {
    let (mut rig, logs) = logging_rig("logs_restart_before", LONG_GRACE);
    rig.join("host-a", true).await;
    rig.join("host-b", true).await;
    logs.settle().await;
    rig.place("job-a", "host-a");
    let mut a = FakeAgent::uploading("host-a");
    let mut b = FakeAgent::uploading("host-b");
    a.take(&rig.beat(&a).await.unwrap());
    b.take(&rig.beat(&b).await.unwrap());
    let old = a.grant.clone().unwrap();
    let held = [old.clone(), b.grant.clone().unwrap()];

    let later = START_MS + 60_000;
    let (mut again, fresh) = logging_over("logs_restart_after", LONG_GRACE, Some(&rig), later);
    again.join("host-a", true).await;
    // host-b's hello is held by the new registry without the route: its first beat mints.
    again.register_uploading("host-b");
    b.grant = None;
    b.stale = true;
    let reply_b = again.beat(&b).await.unwrap();
    assert_eq!(reply_b.log_grant, None);
    fresh.settle().await;
    let reply_a = again.beat(&a).await.unwrap();
    let reply_b = again.beat(&b).await.unwrap();
    let new_a = reply_a.log_grant.expect("host-a reported an old expiry");
    assert!(new_a.expires_at_ms > old.expires_at_ms);
    assert!(reply_b.log_grant.is_some(), "host-b reported none");
    assert_eq!(
        (fresh.minter.calls("host-a"), fresh.minter.calls("host-b")),
        (1, 1)
    );

    let rows = rig.store.scan_prefix("").unwrap();
    let mut stored: Vec<u8> = rows
        .into_iter()
        .flat_map(|(key, row)| {
            let mut bytes = key.into_bytes();
            bytes.extend(row.value);
            bytes
        })
        .collect();
    for dir in [rig.dir(), again.dir()] {
        for file in std::fs::read_dir(dir).unwrap() {
            stored.extend(std::fs::read(file.unwrap().path()).unwrap_or_default());
        }
    }
    let stored = String::from_utf8_lossy(&stored);
    for grant in held.iter().chain([&new_a]) {
        for secret in secrets(grant) {
            assert!(!stored.contains(&secret), "a credential is stored");
        }
    }
}

/// After one of a host's jobs ends and its result is accepted, the host's credential is still
/// held and still renewed and delivered for its other job; once the host is removed from the
/// registry, or falls silent past its grace, it is forgotten and nothing is renewed or sent.
/// Catches one job's end taking the credential from the host's other jobs, and a departed
/// host's credential kept.
#[tokio::test]
async fn the_credential_is_forgotten_when_the_host_leaves_not_when_a_job_ends() {
    let (mut rig, logs) = logging_rig("logs_forgotten_removed", LONG_GRACE);
    rig.join("host-a", true).await;
    logs.settle().await;
    let id = rig.place("job-1", "host-a");
    rig.place("job-2", "host-a");
    let mut agent = FakeAgent::uploading("host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-1");
    agent.run("job-2");
    agent.end("job-1", Outcome::Succeeded, Some(0));
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&id).state, JobState::Succeeded);
    agent.take(&reply);
    rig.beat(&agent).await.unwrap();
    assert!(logs.grants.credential_for("host-a").is_some());
    rig.advance(Duration::from_millis(HALF_LIFE_MS + 1));
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(logs.minter.calls("host-a"), 2);
    assert!(rig.beat(&agent).await.unwrap().log_grant.is_some());

    let host = HostId::new("host-a").unwrap();
    rig.agents.registry.mark_ending(&host);
    rig.agents.registry.remove(&host).unwrap();
    tick(&rig).await;
    assert_eq!(logs.grants.credential_for("host-a"), None);
    rig.advance(Duration::from_millis(HALF_LIFE_MS + 1));
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(logs.minter.calls("host-a"), 2);

    let grace = js_agent_wire::DEFAULT_GRACE_MS;
    let (mut rig, logs) = logging_rig("logs_forgotten_silent", grace);
    rig.join("host-a", true).await;
    logs.settle().await;
    rig.advance(Duration::from_millis(grace));
    tick(&rig).await;
    assert!(
        logs.grants.credential_for("host-a").is_some(),
        "lost at its grace"
    );
    rig.advance(Duration::from_millis(1));
    tick(&rig).await;
    assert_eq!(logs.grants.credential_for("host-a"), None);
    rig.advance(Duration::from_millis(HALF_LIFE_MS));
    tick(&rig).await;
    logs.settle().await;
    assert_eq!(logs.minter.calls("host-a"), 1);
    assert_eq!(logs.grants.counters().hosts_with_credential, 0);
}

/// Job tables over the SQLite file `path`, on `clock`.
fn sqlite(path: &Path, clock: &Arc<StateClock>) -> ServerJobs {
    let clock: Arc<dyn js_state::Clock> = clock.clone();
    let store = SqliteStore::open(path, clock, Options::default()).unwrap();
    let store: Arc<dyn Store> = Arc::new(store);
    js_jobs::JobStore::new(Arc::new(store), crate::job_store::ServerIds::random()).unwrap()
}

/// The ended job `id` listed with its result, as the agent of `host-a` lists it.
fn ended(jobs: &ServerJobs, id: &str, logs: Option<LogReport>) -> HeldJob {
    let allocation = jobs
        .get_allocation(&JobId::parse(id).unwrap())
        .unwrap()
        .unwrap();
    let mut listed = result(
        id,
        &allocation.allocation_id.to_string(),
        Outcome::Succeeded,
        Some(0),
    );
    listed.logs = logs;
    HeldJob {
        job_id: id.to_owned(),
        state: HeldState::ExitedUnacked,
        result: Some(listed),
    }
}

/// A result reporting the lengths of its streams is stored with them through the heartbeat,
/// in the in-memory store and in SQLite, and read back unchanged, after reopening too; a result
/// with no report, and one whose report is inconsistent (more bytes dropped than printed: the
/// heartbeat is still accepted), is stored with none; the retention sweep removes them with the
/// result. The schema change is one new field of the job row, which a build of the previous
/// release keeps and writes back as a field it does not know (js-jobs:
/// `the_log_lengths_are_kept_with_the_result_and_swept_with_it` and
/// `unknown_fields_survive_every_rewrite`). Catches lengths lost, and lengths outliving the
/// result.
#[tokio::test]
async fn the_log_lengths_are_stored_with_the_result_and_swept_with_it() {
    let (mut rig, logs) = logging_rig("logs_lengths", LONG_GRACE);
    rig.join("host-a", true).await;
    logs.settle().await;
    let ids = ["with", "without", "odd"].map(|id| rig.place(id, "host-a"));
    let mut agent = FakeAgent::uploading("host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    let inconsistent = LogReport {
        stdout_dropped: 101,
        stdout_bytes: 100,
        ..REPORT
    };
    agent.end_with_logs("with", REPORT);
    agent.end("without", Outcome::Succeeded, Some(0));
    agent.end_with_logs("odd", inconsistent);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.job_acks.len(), 3);
    let kept = |id: &JobId| rig.job(id).result.and_then(|result| result.logs);
    assert_eq!(kept(&ids[0]), Some(LENGTHS));
    assert_eq!((kept(&ids[1]), kept(&ids[2])), (None, None));
    assert!(
        ids.iter()
            .all(|id| rig.job(id).state == JobState::Succeeded)
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_jobs");
    let dir = TestDir::under(root, "logs_lengths_sqlite");
    let path = dir.path().join("state.db");
    let clock = Arc::new(StateClock::default());
    let jobs = sqlite(&path, &clock);
    for id in ["with", "without", "odd"] {
        placed(&jobs, id, "host-a", spec(), BOOKED);
    }
    let listing = [
        ended(&jobs, "with", Some(REPORT)),
        ended(&jobs, "without", None),
        ended(&jobs, "odd", Some(inconsistent)),
    ];
    let host = HostId::new("host-a").unwrap();
    let context = BeatContext {
        host: &host,
        ceiling: &NetworkCeiling::default(),
        logs: Some(&*logs.grants),
    };
    let served = per_job::serve(&jobs, &context, &listing, &[], 16).unwrap();
    assert_eq!(served.job_acks.len(), 3);
    drop(jobs);
    let jobs = sqlite(&path, &clock);
    let read = |id: &str| {
        let job = jobs.get_job(&JobId::parse(id).unwrap()).unwrap().unwrap();
        job.result.and_then(|result| result.logs)
    };
    assert_eq!(read("with"), Some(LENGTHS));
    assert_eq!((read("without"), read("odd")), (None, None));
    clock.advance(Retention::DEFAULT.window());
    assert_eq!(jobs.tick().unwrap().len(), 3);
    assert!(
        jobs.get_job(&JobId::parse("with").unwrap())
            .unwrap()
            .is_none()
    );
}

/// A start whose sink the agent would refuse (the store's host has a byte the wire's endpoint
/// rule does not allow, though the server's own store client takes it) is sent without the
/// sink, never refused. Catches a malformed sink sent, and a start held back for its sink.
#[tokio::test]
async fn a_sink_the_agent_would_refuse_is_left_out_of_the_start() {
    let (rig, logs) = logging_rig("logs_bad_sink", LONG_GRACE);
    let odd = Endpoint::parse("https://odd_host.example", LOG_REGION, LOG_BUCKET, true).unwrap();
    let minter = Arc::clone(&logs.minter) as Arc<dyn js_logmint::Minter>;
    let grants = grants_over(odd, minter, &status_clock(&rig));
    assert!(grants.sink_for("host-a", "job-a").is_some());
    let (job, allocation) = placed(&rig.jobs, "job-a", "host-a", spec(), BOOKED);
    let host = HostId::new("host-a").unwrap();
    let context = BeatContext {
        host: &host,
        ceiling: &NetworkCeiling::default(),
        logs: Some(&*grants),
    };
    let start = crate::job_start::start_of(&job, &allocation, &context).unwrap();
    assert_eq!(start.log_sink, None);
    let context = BeatContext {
        logs: Some(&*logs.grants),
        ..context
    };
    let start = crate::job_start::start_of(&job, &allocation, &context).unwrap();
    assert_eq!(start.log_sink, Some(sink_of("host-a", "job-a")));
}

/// With every log line captured and the status page rendered, across a mint that succeeds, one
/// the store's STS cannot serve and one it refuses, no access key id, secret access key or
/// session token appears in a line, the page, its JSON, an error's text or a reply's `Debug`
/// text; the page shows the counters. Catches a secret that leaks to an operator's screen.
#[tokio::test]
async fn no_secret_reaches_a_log_a_status_page_or_an_error() {
    let lines = Lines::default();
    let _capture = capture(&lines);
    let (mut rig, logs) = logging_rig("logs_no_secret", LONG_GRACE);
    rig.join("host-a", true).await;
    logs.settle().await;
    let unavailable = MintError::Unavailable(Unavailable::Status {
        status: 503,
        code: "SlowDown".to_owned(),
    });
    let refused = MintError::Refused(Refusal::Status {
        status: 403,
        code: "AccessDenied".to_owned(),
    });
    let errors = [unavailable.to_string(), refused.to_string()];
    logs.minter.fail_next(1, unavailable);
    rig.join("host-b", true).await;
    logs.settle().await;
    logs.minter.fail_next(1, refused);
    rig.join("host-c", true).await;
    logs.settle().await;
    let reply = rig.beat(&FakeAgent::uploading("host-a")).await.unwrap();
    let grant = reply.log_grant.clone().unwrap();
    for host in ["host-b", "host-c"] {
        rig.beat(&FakeAgent::uploading(host)).await.unwrap();
    }

    let registry = Some(rig.agents.registry.clone());
    let status = Status::new(None, registry, None, Vec::new(), status_clock(&rig))
        .with_logs(Some(Arc::clone(&logs.grants)));
    let view = status.view();
    let json = serde_json::to_string(&view).unwrap();
    let shown = page(&view, status.now());
    let counted = r#""logs":{"minted":1,"failed":2,"hosts_with_credential":1,"hosts_failing":2}"#;
    assert!(json.contains(counted), "{json}");
    assert!(shown.contains("<h2>Live logs</h2>"), "{shown}");
    let without = Status::new(None, None, None, Vec::new(), status_clock(&rig)).view();
    assert!(!serde_json::to_string(&without).unwrap().contains("logs"));

    let said = [
        lines.take(),
        json,
        shown,
        format!("{reply:?}"),
        errors.join("\n"),
    ]
    .join("\n");
    assert!(
        said.contains("cannot mint a host's log credential"),
        "{said}"
    );
    for secret in secrets(&grant) {
        assert!(!said.contains(&secret), "{said}");
    }
    assert!(starts(&reply).is_empty());
}

/// The rig's clock, as the status and the grants read it.
fn status_clock(rig: &Rig) -> SharedClock {
    rig.shared.clone()
}
