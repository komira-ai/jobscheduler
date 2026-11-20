//! The host's session as the store row `node/<host>` ([`super`]): what a hello, a heartbeat and
//! the listing's settle write and read, over servers that share one store that can fail a read
//! or a commit. Nothing waits on time: the clock is manual and every interleaving is a hook of
//! the store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use js_agent_wire::{HeldJob, InstanceIdentity};
use js_jobs::interfere::{INJECTED, Interfere};
use js_jobs::{JobId, JobStore, NodeHello, NodeRow, Retention};
use js_registry::{PoolConfig, Registry, SessionSource};
use js_state::fake::Fake;
use js_state::{ManualClock as StoreClock, Store};
use js_testkit::dir::TestDir;
use js_types::{ProviderId, ResourceClass, Resources, Shape};
use serde_json::{Value, json};

use super::super::*;
use super::*;
use crate::agent_api::job_rig::{OneClock, tables_on};
use crate::clock::SharedClock;
use crate::job_store::ServerIds;

#[path = "agent_api_rows_beat_tests.rs"]
mod beat;

#[path = "agent_api_rows_binding_tests.rs"]
mod binding;

/// Session secrets that start at a number of the server's own, so that two servers never
/// make the same secret and a secret a server checks is the one the row's digest holds.
struct Secrets(AtomicU64);

impl SessionSource for Secrets {
    fn fresh(&self) -> Option<[u8; 32]> {
        let n = self.0.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&n.to_be_bytes());
        Some(bytes)
    }
}

/// What a server of a [`World`] has beyond the registry: the job tables, the scheduler, a
/// state store for its bindings.
#[derive(Clone, Copy)]
pub(super) struct Kind {
    pub(super) jobs: bool,
    pub(super) placing: bool,
    pub(super) kept: bool,
}

impl Kind {
    /// A server with the job tables that places no job.
    pub(super) const JOBS: Kind = Kind {
        jobs: true,
        placing: false,
        kept: false,
    };
    /// A server with the job tables that places queued jobs on a heartbeat and a tick.
    pub(super) const PLACING: Kind = Kind {
        placing: true,
        ..Kind::JOBS
    };
    /// A server with the job tables that keeps its bindings in the store.
    pub(super) const KEPT: Kind = Kind {
        kept: true,
        ..Kind::JOBS
    };
    /// A server without the job tables that keeps its bindings in the store.
    pub(super) const BINDINGS: Kind = Kind {
        jobs: false,
        placing: false,
        kept: true,
    };
    /// A server without the job tables.
    pub(super) const PLAIN: Kind = Kind {
        jobs: false,
        placing: false,
        kept: false,
    };
}

/// One store, one clock, and the servers made over them.
pub(super) struct World {
    pub(super) store: std::sync::Arc<Interfere<Fake>>,
    pub(super) clock: std::sync::Arc<StoreClock>,
    /// A handle on the job tables of its own, for what a test reads and writes.
    pub(super) jobs: ServerJobs,
    next: AtomicU64,
}

/// The job tables of a new server over `store`.
fn tables_over(store: &std::sync::Arc<Interfere<Fake>>) -> ServerJobs {
    let dynamic: std::sync::Arc<dyn Store> = store.clone();
    let jobs = JobStore::new(std::sync::Arc::new(dynamic), ServerIds::random()).unwrap();
    jobs.with_retention(Retention::DEFAULT)
}

/// A server of a [`World`]: its agent side and the credential its pool's hosts use.
pub(super) struct Server {
    pub(super) agents: Arc<Agents>,
    pub(super) who: Credential,
    _dir: TestDir,
}

/// The session a hello opened, as the host's agent holds it.
#[derive(Clone)]
pub(super) struct Sess {
    pub(super) host: String,
    pub(super) epoch: u64,
    pub(super) secret: String,
}

impl World {
    pub(super) fn new() -> World {
        quiet_logs();
        let clock = std::sync::Arc::new(StoreClock::default());
        let (store, jobs) = tables_on(clock.clone(), Retention::DEFAULT);
        // The first handle's own tables are the test's; the servers make their own.
        World {
            store,
            clock,
            jobs,
            next: AtomicU64::new(1),
        }
    }

    /// A server named `name` of `kind` over the world's store and clock.
    pub(super) fn server(&self, name: &str, kind: Kind) -> Server {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_rows");
        let dir = TestDir::under(root, name);
        let shared = SharedClock::new(OneClock(self.clock.clone()));
        let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".into())).unwrap();
        let arch = ("arch".to_owned(), "x86_64".to_owned());
        let shape = Shape::new(resources, [arch].into_iter().collect());
        let provider = ProviderId::new("burst").unwrap();
        let pool = PoolConfig::new(provider, ResourceClass::gpu_host(), shape, 4);
        let pool = pool.placing_by_hello();
        let placer = std::sync::Arc::new(crate::sched::place::SchedPlacer::new(
            crate::sched::engine::Settings::default(),
            shared.clone(),
        ));
        // Each server's secrets start at a number of its own.
        let base = self.next.fetch_add(1000, Ordering::Relaxed);
        let secrets = Secrets(AtomicU64::new(base));
        let registry = Registry::with_sessions(pool, shared.clone(), secrets, placer.clone());
        let mut agents = Agents::over(registry, &shared, dir.path());
        if kind.kept {
            let dynamic: std::sync::Arc<dyn Store> = self.store.clone();
            agents = agents.kept_in(dynamic);
        }
        if kind.jobs {
            agents = agents.with_jobs(std::sync::Arc::new(tables_over(&self.store)));
        }
        if kind.placing {
            agents = agents.with_placer(placer);
        }
        let who = agents.tokens.identify(&"j".repeat(40)).unwrap();
        Server {
            agents: Arc::new(agents),
            who,
            _dir: dir,
        }
    }

    /// Moves the clock by `by`.
    pub(super) fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The row of `host`, which must exist.
    pub(super) fn row(&self, host: &str) -> NodeRow {
        self.jobs.read_node(&id(host)).unwrap().unwrap()
    }

    /// The id of the allocation of the job `job`, as text.
    pub(super) fn alloc(&self, job: &str) -> String {
        let allocation = self.jobs.get_allocation(&JobId::parse(job).unwrap());
        allocation.unwrap().unwrap().allocation_id.to_string()
    }

    /// How many transactions the store has committed.
    pub(super) fn commits(&self) -> u64 {
        self.store.commit_stats().txns
    }
}

/// The host id `host`.
pub(super) fn id(host: &str) -> HostId {
    HostId::new(host).unwrap()
}

/// The job id `job`.
pub(super) fn job(job: &str) -> JobId {
    JobId::parse(job).unwrap()
}

/// A process-wide subscriber that takes every event and writes it nowhere, so the code of each
/// log line on a failure path runs, on whichever thread reaches it.
fn quiet_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .try_init();
    });
    tracing::callsite::rebuild_interest_cache();
}

/// The body of the hello of `host`, which reports the `per-job` capability.
pub(super) fn hello_body(host: &str) -> Bytes {
    let hello = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "instance": { "provider": "lambdalabs", "instance_id": "i-1" },
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "capabilities": { "per-job": "true", "arch": "x86_64" },
        "phase": "serving",
    });
    Bytes::from(hello.to_string())
}

/// The body of a heartbeat of `session` with the sequence `seq`, listing `jobs`, carrying
/// `extra` (an object whose fields are added).
pub(super) fn beat_body(
    session: &Sess,
    seq: Option<u64>,
    jobs: Vec<HeldJob>,
    extra: Value,
) -> Bytes {
    let mut beat = json!({
        "host_id": session.host,
        "epoch": session.epoch,
        "session": session.secret,
        "phase": "serving",
        "jobs": jobs,
    });
    if let Some(seq) = seq {
        beat["seq"] = json!(seq);
    }
    for (key, value) in extra.as_object().into_iter().flatten() {
        beat[key] = value.clone();
    }
    Bytes::from(beat.to_string())
}

/// The route's answer to the heartbeat `body`, for `agents` and `who`.
pub(super) async fn post_beat(
    agents: Arc<Agents>,
    who: Credential,
    body: Bytes,
) -> Result<HeartbeatReply, Refused> {
    let answer = heartbeat(State(agents), Extension(who), (None, None), Ok(body));
    answer.await.map(|Json(reply)| reply)
}

/// What a refusal says: its status and its code.
pub(super) fn said(refused: &Refused) -> (u16, &'static str) {
    (refused.status.as_u16(), refused.error)
}

impl Server {
    /// The hello of `host` through the route, with the pool's join token.
    pub(super) async fn hello(&self, host: &str) -> Result<Welcome, Refused> {
        let body = Ok(hello_body(host));
        let welcome = serve_hello(&self.agents, self.who.clone(), None, body, &|| ());
        welcome.await.map(|Json(welcome)| welcome)
    }

    /// The hello of `host`, which must be admitted, as the session it opened.
    pub(super) async fn join(&self, host: &str) -> Sess {
        let welcome = self.hello(host).await.unwrap();
        Sess {
            host: host.to_owned(),
            epoch: welcome.epoch,
            secret: welcome.session,
        }
    }

    /// The heartbeat of `session` with the sequence `seq` listing `jobs`.
    pub(super) async fn beat(
        &self,
        session: &Sess,
        seq: Option<u64>,
        jobs: Vec<HeldJob>,
    ) -> Result<HeartbeatReply, Refused> {
        self.beat_with(session, seq, jobs, json!({})).await
    }

    /// [`Server::beat`] with the fields of `extra` added to the heartbeat.
    pub(super) async fn beat_with(
        &self,
        session: &Sess,
        seq: Option<u64>,
        jobs: Vec<HeldJob>,
        extra: Value,
    ) -> Result<HeartbeatReply, Refused> {
        let body = beat_body(session, seq, jobs, extra);
        post_beat(Arc::clone(&self.agents), self.who.clone(), body).await
    }

    /// The epoch of the session this server's registry holds of `host`.
    pub(super) fn holds(&self, host: &str) -> Option<u64> {
        let own = self.agents.registry.own_host(&id(host));
        own.map(|own| own.host.epoch)
    }
}

/// The heartbeat of `host` in the epoch `epoch` (a session secret the registry never made),
/// for the calls that look at the epoch alone.
fn plain_beat(host: &str, epoch: u64) -> Heartbeat {
    let body = beat_body(
        &Sess {
            host: host.to_owned(),
            epoch,
            secret: "s".into(),
        },
        Some(1),
        Vec::new(),
        json!({}),
    );
    serde_json::from_slice(&body).unwrap()
}

/// The hello of `host`, decoded.
fn hello_of(host: &str) -> Hello {
    serde_json::from_slice(&hello_body(host)).unwrap()
}

/// The instance a one-time token is minted for.
fn instance() -> InstanceIdentity {
    InstanceIdentity {
        provider: "lambdalabs".to_owned(),
        instance_id: "i-1".to_owned(),
    }
}

/// Asserts that every call of the module does nothing when the server holds no job tables: the
/// registry's session is the host's session, nothing is written, a beat is applied to no row,
/// and a host's listing is clean. Catches a server without the job tables that reads or
/// writes a row it has no tables for.
#[tokio::test]
async fn without_the_job_tables_no_call_reads_or_writes_a_row() {
    let world = World::new();
    let server = world.server("rows_plain", Kind::PLAIN);
    let commits = world.commits();
    let host = id("host-a");
    refresh(&server.agents, &host).await.unwrap();
    refresh_for_beat(&server.agents, &plain_beat("host-a", 7))
        .await
        .unwrap();
    let hello = hello_of("host-a");
    let mut welcome = server.agents.registry.hello(&hello, &server.who).unwrap();
    let epoch = welcome.epoch;
    record_hello(&server.agents, &host, &hello, &mut welcome).unwrap();
    hello_row(&server.agents, &host, &hello, &mut welcome)
        .await
        .unwrap();
    assert_eq!(welcome.epoch, epoch, "the registry's epoch stands");
    let applied = beat_row(&server.agents, &plain_beat("host-a", epoch)).await;
    assert!(applied.unwrap().is_none());
    assert!(server.agents.listing_clean(&host));
    assert_eq!(world.commits(), commits, "nothing was written");
}

/// Asserts that a hello's epoch is the store's: a host whose row holds epoch 2 says hello and
/// is given 3 by the welcome, the registry and the row, which also holds the digest of the
/// welcome's secret (never the secret), the pool, the first listing still pending and the
/// session's phase. Catches a welcome whose epoch is the registry's own count, which another
/// server's hello of the host can have used already.
#[tokio::test]
async fn a_hello_takes_the_epoch_the_store_numbers() {
    let world = World::new();
    let server = world.server("rows_epoch", Kind::JOBS);
    let hello = NodeHello {
        session: None,
        pool: "burst".to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        phase: js_agent_wire::Phase::Serving,
        agent_process: None,
    };
    world.jobs.hello_node(&id("host-a"), &hello).unwrap();
    world.jobs.hello_node(&id("host-a"), &hello).unwrap();
    // The host has been silent since: its row is no live session, and a hello is admitted.
    world.advance(Duration::from_secs(3600));

    let welcome = server.hello("host-a").await.unwrap();
    assert_eq!(welcome.epoch, 3);
    assert_eq!(server.holds("host-a"), Some(3));
    let row = world.row("host-a");
    assert_eq!(row.record.epoch, 3);
    let digest = session_digest(&welcome.session);
    assert_eq!(row.record.session.unwrap().as_bytes(), &digest);
    assert_eq!(row.record.pool, "burst");
    assert!(row.record.state.first_listing_pending);
    assert!(row.record.claim.is_some() && row.record.shape.is_some());
}

/// Asserts that a hello whose row cannot be written is refused 503 `state-write-failed`, that
/// the registry gives its session up (the retried hello is admitted, not refused `host-live`),
/// that the one-time token's use is given back (the same token admits the retry), and that no
/// row is left by the failure. Catches a refused hello that leaves the registry holding a
/// session no row backs, which makes the agent's retry `host-live` for as long as it beats
/// nothing, and a token that is spent by a hello that was never served.
#[tokio::test]
async fn a_hello_whose_row_cannot_be_written_leaves_nothing_held() {
    let world = World::new();
    let server = world.server("rows_hello_fails", Kind::JOBS);
    let minted = server
        .agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let token = Credential::of_token(&minted.token);
    let hello = || {
        let body = Ok(hello_body("host-a"));
        let welcome = serve_hello(
            &server.agents,
            token.clone(),
            Some(token.clone()),
            body,
            &|| (),
        );
        async move { welcome.await.map(|Json(welcome)| welcome) }
    };

    world.store.fail_after(0);
    let refused = hello().await.expect_err("the row cannot be written");
    assert_eq!(said(&refused), (503, "state-write-failed"));
    assert_eq!(world.jobs.read_node(&id("host-a")).unwrap(), None);

    let welcome = hello()
        .await
        .expect("not host-live, and the token is not spent");
    assert_eq!(welcome.epoch, 1);
    assert_eq!(server.holds("host-a"), Some(1));
}

/// Asserts that a server whose registry does not hold a host takes the session its row holds:
/// the epoch (so a heartbeat of it is served), and, with a state store, the host's binding with
/// the taint it reported on the other server; a host with a binding and no row takes the
/// binding alone; a host with neither is left unknown. Catches a refresh that adopts nothing
/// (the beat is then `unknown-host`), one that drops the stored taint (the host is offered
/// jobs its agent refuses), and one that invents a session for a host nobody said hello for.
#[tokio::test]
async fn a_server_takes_the_session_and_binding_the_store_holds() {
    let world = World::new();
    let first = world.server("rows_refresh_first", Kind::KEPT);
    let second = world.server("rows_refresh_second", Kind::KEPT);
    let session = first.join("host-a").await;
    let taint = json!({ "reason": "residue", "check": "daemonised-child", "run_id": "r1" });
    let beat = first.beat_with(&session, Some(1), Vec::new(), json!({ "taint": taint }));
    assert!(beat.await.unwrap().taint_ack);

    assert_eq!(second.holds("host-a"), None);
    refresh(&second.agents, &id("host-a")).await.unwrap();
    assert_eq!(second.holds("host-a"), Some(session.epoch));
    let bindings = second.agents.registry.bindings();
    let held = bindings[&id("host-a")].taint().unwrap();
    assert_eq!(held.run_id.as_deref(), Some("r1"));

    // A host with a stored binding and no row (a server without the tables said hello).
    let plain = world.server("rows_refresh_plain", Kind::BINDINGS);
    let other = plain.join("host-b").await;
    let taint = json!({ "reason": "residue", "check": "daemonised-child", "run_id": "r2" });
    let beat = plain.beat_with(&other, Some(1), Vec::new(), json!({ "taint": taint }));
    assert!(beat.await.unwrap().taint_ack);
    refresh(&second.agents, &id("host-b")).await.unwrap();
    let bindings = second.agents.registry.bindings();
    assert_eq!(
        bindings[&id("host-b")].taint().unwrap().run_id.as_deref(),
        Some("r2")
    );
    assert_eq!(world.jobs.read_node(&id("host-b")).unwrap(), None);

    // A host the store knows nothing of.
    refresh(&second.agents, &id("host-c")).await.unwrap();
    assert_eq!(second.holds("host-c"), None);
}

/// Asserts that a row without a binding is adopted too, and that the adopted session is the
/// row's: the heartbeat's secret is checked against the row's digest (a wrong one is refused
/// `session-mismatch`, 403), and a heartbeat of the session is served by the server that did
/// not take the hello. Catches a server that serves a beat of a session it adopted without
/// checking its secret.
#[tokio::test]
async fn an_adopted_session_is_checked_against_the_rows_digest() {
    let world = World::new();
    let first = world.server("rows_digest_first", Kind::JOBS);
    let second = world.server("rows_digest_second", Kind::JOBS);
    let session = first.join("host-a").await;
    let wrong = Sess {
        secret: "not-the-secret".to_owned(),
        ..session.clone()
    };
    let refused = second.beat(&wrong, Some(1), Vec::new()).await.unwrap_err();
    assert_eq!(said(&refused), (403, "session-mismatch"));
    second.beat(&session, Some(1), Vec::new()).await.unwrap();
    assert_eq!(second.holds("host-a"), Some(session.epoch));
}

/// Asserts that a call that cannot read what the store holds of the host is refused, not
/// judged on less: with the row unreadable a hello is 503 `state-write-failed` and the
/// registry holds nothing; a heartbeat of an epoch the registry does not hold is the same; a
/// store whose clock cannot be read refuses the same way. Catches a hello admitted over a row
/// nobody read (it would take another server's epoch) and a beat refused as unknown when the
/// store was only unreachable (the agent would say hello again and lose its runs).
#[tokio::test]
async fn a_row_that_cannot_be_read_refuses_the_hello_and_the_beat() {
    let world = World::new();
    let first = world.server("rows_unread_first", Kind::JOBS);
    let second = world.server("rows_unread_second", Kind::JOBS);
    let session = first.join("host-a").await;

    world.store.fail_read_after(0);
    let refused = second.hello("host-b").await.unwrap_err();
    assert_eq!(said(&refused), (503, "state-write-failed"));
    assert_eq!(second.holds("host-b"), None);

    world.store.fail_read_after(0);
    let refused = second
        .beat(&session, Some(1), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(said(&refused), (503, "state-write-failed"));
    assert_eq!(second.holds("host-a"), None);

    world.store.fail_clock(true);
    let failed = refresh(&second.agents, &id("host-a")).await.unwrap_err();
    assert!(failed.to_string().contains(INJECTED), "{failed}");
    world.store.fail_clock(false);
    second.beat(&session, Some(1), Vec::new()).await.unwrap();
    assert_eq!(second.holds("host-a"), Some(session.epoch));
}

/// Asserts that a heartbeat of an epoch the registry holds reads nothing of the store for the
/// session (only the row write follows), that one of an epoch it does not hold reads the row,
/// and that one whose host id is not an id is left for the registry to refuse. Catches a beat
/// that reads the row and the binding on every call (two more reads per beat on every
/// server), and a refresh that fails a beat whose id the registry would have refused with its
/// own code.
#[tokio::test]
async fn a_beat_of_the_epoch_the_registry_holds_reads_no_row_first() {
    let world = World::new();
    let first = world.server("rows_reads_first", Kind::JOBS);
    let second = world.server("rows_reads_second", Kind::JOBS);
    let session = first.join("host-a").await;
    world.store.take_reads();

    refresh_for_beat(&first.agents, &plain_beat("host-a", session.epoch))
        .await
        .unwrap();
    assert_eq!(world.store.take_reads(), Vec::new());

    refresh_for_beat(&second.agents, &plain_beat("host-a", session.epoch))
        .await
        .unwrap();
    assert!(!world.store.take_reads().is_empty());
    assert_eq!(second.holds("host-a"), Some(session.epoch));

    refresh_for_beat(&second.agents, &plain_beat("not a host!", 1))
        .await
        .unwrap();
    assert_eq!(world.store.take_reads(), Vec::new());
}

/// Asserts what each judgement of the row's beat answers: an applied beat leaves its phase,
/// load and sequence in the row; a beat whose sequence the row has passed is answered 200 with
/// no verdict on any result and writes nothing; a beat of an epoch a later hello passed is 409
/// `stale-epoch`; a beat whose row is gone is 409 `unknown-host`; a beat whose write fails is
/// 503 `state-write-failed` and the next one is served. Catches a late beat that is applied or
/// refused, a stale epoch served from the row, a host without a row served as if it had a
/// session, and a store failure answered as a refusal that makes the agent say hello.
#[tokio::test]
async fn each_judgement_of_a_beat_is_answered_as_the_row_says() {
    let world = World::new();
    let server = world.server("rows_judgements", Kind::JOBS);
    let session = server.join("host-a").await;
    let load = json!({
        "seq": 4, "mono_usec": 5, "cpu_usage_usec": 6,
        "working_set_bytes": 7, "active_file_bytes": 8, "actions_cpu_limited": true,
    });
    let extra = json!({ "load": load });
    server
        .beat_with(&session, Some(5), Vec::new(), extra)
        .await
        .unwrap();
    let state = world.row("host-a").record;
    let reading = state.state.load.unwrap();
    assert_eq!(
        (reading.seq, reading.mono_usec, reading.cpu_usage_usec),
        (4, 5, 6)
    );
    assert_eq!(
        (reading.working_set_bytes, reading.active_file_bytes),
        (7, 8)
    );
    assert!(reading.actions_cpu_limited);
    assert_eq!(state.last_beat.unwrap().seq, Some(5));

    let commits = world.commits();
    for seq in [5, 3] {
        let reply = server.beat(&session, Some(seq), Vec::new()).await.unwrap();
        assert!(reply.job_acks.is_empty());
    }
    assert_eq!(world.commits(), commits, "a passed beat writes nothing");
    assert_eq!(world.row("host-a").record.last_beat.unwrap().seq, Some(5));

    world.store.fail_after(0);
    let refused = server
        .beat(&session, Some(6), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(said(&refused), (503, "state-write-failed"));
    server.beat(&session, Some(6), Vec::new()).await.unwrap();

    // A later hello of the host passed the epoch the registry holds.
    let row = world.row("host-a");
    let next = NodeHello {
        session: row.record.session,
        pool: "burst".to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        phase: js_agent_wire::Phase::Serving,
        agent_process: None,
    };
    world.jobs.hello_node(&id("host-a"), &next).unwrap();
    let refused = server
        .beat(&session, Some(7), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(said(&refused), (409, "stale-epoch"));
    let elsewhere = world.server("rows_judgements_other", Kind::JOBS);
    let refused = elsewhere
        .beat(&session, Some(7), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(said(&refused), (409, "stale-epoch"));
}

/// Asserts that a beat of a session whose row is gone is 409 `unknown-host`, which makes the
/// agent say hello: the row is deleted at its version, and the beat, whose epoch the registry
/// holds, is refused. Catches a beat served for a host whose row is not there, which would run
/// its listing against jobs of a session nobody holds.
#[tokio::test]
async fn a_beat_whose_row_is_gone_is_unknown_host() {
    let world = World::new();
    let server = world.server("rows_gone", Kind::JOBS);
    let session = server.join("host-a").await;
    let row = world.row("host-a");
    assert!(world.jobs.forget_node(&id("host-a"), row.version).unwrap());
    let refused = server
        .beat(&session, Some(1), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(said(&refused), (409, "unknown-host"));
}

/// Asserts what [`settle`] writes: the first listing is applied, the listing's cleanliness and
/// the missing counts, at the version the beat wrote; a row that a newer beat moved is left as
/// that beat wrote it; a write that fails is logged and the flags stay as they were. Catches a
/// settle that overwrites a newer beat's row with an older listing's flags, and one that
/// reports a flag written that was not.
#[test]
fn settle_writes_the_flags_at_the_version_the_beat_wrote() {
    let world = World::new();
    let server = world.server("rows_settle", Kind::JOBS);
    let jobs = server.agents.jobs.clone().unwrap();
    let host = id("host-a");
    let welcome = {
        let hello = hello_of("host-a");
        let mut welcome = server.agents.registry.hello(&hello, &server.who).unwrap();
        record_hello(&server.agents, &host, &hello, &mut welcome).unwrap();
        welcome
    };
    let applied = |seq| {
        let beat = jobs.beat_node(&host, welcome.epoch, Some(seq), |row| row.state.clone());
        match beat.unwrap() {
            js_jobs::Beat::Applied(row) => row,
            other => panic!("{other:?}"),
        }
    };
    let row = applied(1);
    assert!(row.record.state.first_listing_pending);

    let missing = BTreeMap::from([(job("job-a"), 1)]);
    settle(&jobs, &row, false, missing.clone());
    let state = world.row("host-a").record.state;
    assert!(!state.first_listing_pending);
    assert!(!state.last_listing_clean);
    assert_eq!(state.missing_in_a_row, missing);

    // A newer beat moved the row: this settle, of the version it wrote before, writes nothing.
    let newer = applied(2);
    assert_ne!(newer.version, row.version);
    settle(&jobs, &row, true, BTreeMap::new());
    assert_eq!(world.row("host-a"), newer);

    // A failed write leaves the flags as they were.
    world.store.fail_after(0);
    settle(&jobs, &newer, true, BTreeMap::new());
    assert_eq!(world.row("host-a"), newer);
}

/// Asserts that a listing whose settle cannot be written is still answered 200, and that the
/// row keeps the first listing pending, so the next beat's listing is judged as the first
/// again and writes the flags. Catches a beat refused because its flags could not be written
/// (the agent's whole account is lost with it), and a flag that is cleared when the write
/// failed.
#[tokio::test]
async fn a_settle_that_fails_leaves_the_first_listing_pending() {
    let world = World::new();
    let server = world.server("rows_settle_fails", Kind::JOBS);
    let session = server.join("host-a").await;
    // The beat's own write passes; the settle after the listing is the next commit.
    world.store.fail_after(1);
    server.beat(&session, Some(1), Vec::new()).await.unwrap();
    assert!(world.row("host-a").record.state.first_listing_pending);
    server.beat(&session, Some(2), Vec::new()).await.unwrap();
    assert!(!world.row("host-a").record.state.first_listing_pending);
}

/// Asserts what a host's listing flag is for a caller that reads it: true without a row and
/// for a host that lists only its own jobs, false after a listing that names a job the host
/// holds no allocation for, and false when the row cannot be read. Catches a flag that is read
/// as clean when the store failed (a placement on a host that runs what the store does not
/// know).
#[tokio::test]
async fn the_listing_flag_is_the_rows_and_an_unread_row_is_unclean() {
    let world = World::new();
    let server = world.server("rows_clean", Kind::JOBS);
    let host = id("host-a");
    assert!(server.agents.listing_clean(&host), "no row");
    let session = server.join("host-a").await;
    server.beat(&session, Some(1), Vec::new()).await.unwrap();
    assert!(server.agents.listing_clean(&host));
    let ghost = HeldJob {
        job_id: "ghost".to_owned(),
        state: js_agent_wire::HeldState::Running,
        result: None,
    };
    let reply = server.beat(&session, Some(2), vec![ghost]).await.unwrap();
    assert_eq!(crate::agent_api::job_rig::stops(&reply), ["ghost"]);
    assert!(!server.agents.listing_clean(&host));
    server.beat(&session, Some(3), Vec::new()).await.unwrap();
    assert!(server.agents.listing_clean(&host));
    world.store.fail_read_after(0);
    assert!(
        !server.agents.listing_clean(&host),
        "an unread row is not clean"
    );
}
