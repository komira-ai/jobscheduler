//! A server with the job tables over a store that can fail a commit, hosts registered with or
//! without the `per-job` capability, and a fake of an agent that speaks the per-job messages,
//! for the tests of the heartbeat's per-job half and of the job pass. Nothing here waits on
//! time: the registry's clock and the store's clock are manual. In a rig made by
//! [`clocked_rig`] they are one clock a test moves ([`Rig::advance`]); in one made by
//! [`logging_rig`] the test moves the registry's, and the store keeps a clock of its own.
//!
//! A server with a log sink ([`logging_rig`]) mints its host credentials with js-logmint's
//! fake minter over js-logstore's fake store, both on the server's clock; its hosts say hello
//! through the route ([`Rig::join`]), and the fake agent can report `log-upload` and the
//! credential it holds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use js_agent_wire::{
    Command, DEFAULT_HEARTBEAT_INTERVAL_MS, ExclusiveRefusal, HeldJob, HeldState, JobResult,
    JobStartSpec, LogGrant, LogGrantState, LogTail, Outcome, Phase, RefusalReason, Taint,
};
use js_jobs::interfere::Interfere;
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, Constraints, JobId, JobRecord, JobSpec, JobState,
    JobStore, Needs, NetworkPolicy, NewJob, Retention,
};
use js_logmint::fake::FakeMinter;
use js_logmint::{Bucket, LogPrefix};
use js_logstore::fake::FakeStore;
use js_logstore::{Endpoint, Store as LogStore};
use js_registry::{PoolConfig, Registry, SequentialSessions, WholeNodeConfig};
use js_sched::Backfill;
use js_state::fake::{Fake, Flaw};
use js_state::{ManualClock as StoreClock, Store};
use js_testkit::dir::TestDir;
use js_types::{ImageDigestRef, Labels, ProviderId, ResourceClass, Resources, Shape};
use js_types::{ManualClock, Timestamp};
use serde_json::json;

use super::*;
use crate::clock::SharedClock;
use crate::job_expiry::JobLimits;
use crate::logs::{Scope, SinkTemplate, StoreClock as LogClock};

/// What [`Rig::place`] books: a share of a host with no GPU.
pub(crate) const BOOKED: Booked = Booked::Shape {
    cpu_millis: 1500,
    memory_bytes: 1 << 30,
    disk_bytes: 2 << 30,
    accelerators: 0,
};

/// The image every job runs.
pub(crate) fn image() -> String {
    format!("team/app@sha256:{}", "a".repeat(64))
}

/// A job that runs `run --fast` from [`image`] for at most ten minutes, with no network.
pub(crate) fn spec() -> JobSpec {
    JobSpec {
        image: ImageDigestRef::parse(&image()).ok(),
        command: vec!["run".to_owned()],
        args: vec!["--fast".to_owned()],
        env: BTreeMap::new(),
        needs: Needs::Shape(Resources::new(1500, 1 << 30, 0, None).unwrap()),
        constraints: Constraints::default(),
        wall_limit_secs: 600,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: js_jobs::JobFiles::default(),
        labels: Labels::default(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
    }
}

/// Job tables of the server's own type over a fresh store that can fail a commit, so a test
/// without a server runs the same code a heartbeat does.
pub(crate) fn tables() -> (Arc<Interfere<Fake>>, ServerJobs) {
    tables_on(Arc::default(), Retention::DEFAULT)
}

/// [`tables`] over a store whose clock is `clock`, keeping each ended job for `retention`.
pub(crate) fn tables_on(
    clock: Arc<StoreClock>,
    retention: Retention,
) -> (Arc<Interfere<Fake>>, ServerJobs) {
    let store = Arc::new(Interfere::new(Fake::with_flaw(clock, Flaw::None)));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = JobStore::new(Arc::new(dynamic), crate::job_store::ServerIds::random()).unwrap();
    (store, jobs.with_retention(retention))
}

/// When a rig's clock starts: the registry's reading, in milliseconds.
pub(crate) const START_MS: u64 = 5_000_000;

/// The store's manual clock read as the server's clock: both count from the Unix epoch, so a
/// rig made by [`clocked_rig`] has one time for the registry, the server and the store.
pub(crate) struct OneClock(pub(crate) Arc<StoreClock>);

impl js_types::Clock for OneClock {
    fn now(&self) -> Timestamp {
        let since = js_state::Clock::now(&*self.0).as_duration();
        Timestamp::from_millis(u64::try_from(since.as_millis()).unwrap())
    }
}

/// Records the job `id` with `spec` and places it on `host` with `booked`; its records.
pub(crate) fn placed(
    jobs: &ServerJobs,
    id: &str,
    host: &str,
    spec: JobSpec,
    booked: Booked,
) -> (JobRecord, AllocationRecord) {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    jobs.place(&id, js_types::HostId::new(host).unwrap(), booked)
        .unwrap()
}

/// The server under test and the store under it.
pub(crate) struct Rig {
    pub(crate) agents: Arc<Agents>,
    /// The clock [`Rig::advance`] moves: the store's; in a rig made by [`clocked_rig`], also
    /// the registry's and the server's. In a rig made by [`logging_rig`] it is the registry's,
    /// the server's and the minter's, and the store has a clock of its own, which only the
    /// store's syncs move, so that a commit does not move the time the log tests count.
    pub(crate) clock: Arc<StoreClock>,
    /// The server's clock.
    pub(crate) shared: SharedClock,
    /// The job tables the server holds (also when the server is not given them).
    pub(crate) jobs: Arc<ServerJobs>,
    /// The store under the job tables, which fails the commit it is told to.
    pub(crate) store: Arc<Interfere<Fake>>,
    who: Credential,
    sessions: BTreeMap<String, (u64, String)>,
    /// The GPUs of each host the rig's hosts announce.
    gpus: u64,
    _dir: TestDir,
}

/// The store a log sink names, as the agent and the server see it.
pub(crate) const LOG_ENDPOINT: &str = "https://objects.example";

/// The region of [`LOG_ENDPOINT`].
pub(crate) const LOG_REGION: &str = "us-east-1";

/// The bucket of the log sink.
pub(crate) const LOG_BUCKET: &str = "logs";

/// The log prefix of the sink: one the prefix `a/bc/` is outside of.
pub(crate) const LOG_PREFIX: &str = "a/b";

/// How long a host credential is asked to live.
pub(crate) const LOG_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// A server named `name` that holds the job tables and minting through a fake minter, whose
/// hosts are lost after `grace_ms` of silence; the fake minter and the server's grants.
pub(crate) fn logging_rig(name: &str, grace_ms: u64) -> (Rig, Logs) {
    logging_over(name, grace_ms, None, START_MS)
}

/// [`logging_rig`] over the job tables of `tables` (a store a server ran on before) when
/// given (and on its clock), with the clock at `now_ms`: a server rebuilt on the same store,
/// with a new registry and nothing of the log credentials held before. Its registry, server
/// and minter read one manual clock, which [`Rig::advance`] moves; fresh job tables get a
/// clock of their own, which a commit's sync moves and nothing else.
pub(crate) fn logging_over(
    name: &str,
    grace_ms: u64,
    tables: Option<&Rig>,
    now_ms: u64,
) -> (Rig, Logs) {
    let clock = tables.map_or_else(Arc::default, |rig| Arc::clone(&rig.clock));
    let since = js_state::Clock::now(&*clock).as_duration();
    let target = std::time::Duration::from_millis(now_ms);
    clock.advance(target.saturating_sub(since));
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
    let store = FakeStore::new(endpoint.clone(), Arc::new(LogClock(shared.clone())));
    let minter = Arc::new(FakeMinter::new(store));
    let grants = grants_over(
        endpoint,
        Arc::clone(&minter) as Arc<dyn js_logmint::Minter>,
        &shared,
    );
    let extra = Extra {
        grace_ms: Some(grace_ms),
        logs: Some(Arc::clone(&grants)),
        tables: tables.map(|rig| (Arc::clone(&rig.store), Arc::clone(&rig.jobs))),
        store_clock: Some(Arc::default()),
        whole: None,
    };
    let kept = (JobLimits::default(), Retention::DEFAULT);
    let rig = rig_on(name, (true, false), kept, (clock, shared.clone()), extra);
    let logs = Logs {
        minter,
        grants,
        clock: shared,
    };
    (rig, logs)
}

/// The grants of a sink at `endpoint` under [`LOG_PREFIX`], minting with `minter` on `clock`.
pub(crate) fn grants_over(
    endpoint: Endpoint,
    minter: Arc<dyn js_logmint::Minter>,
    clock: &SharedClock,
) -> Arc<LogGrants> {
    let prefix = LogPrefix::new(LOG_PREFIX).unwrap();
    let template = SinkTemplate {
        endpoint,
        prefix: prefix.clone(),
    };
    let scope = Scope {
        bucket: Bucket::new(LOG_BUCKET).unwrap(),
        prefix,
        ttl: LOG_TTL,
    };
    Arc::new(LogGrants::new(template, scope, minter, clock.clone()))
}

/// The log side of a [`logging_rig`].
pub(crate) struct Logs {
    /// The fake minter, which counts the mints per host and can fail or hold them.
    pub(crate) minter: Arc<FakeMinter>,
    /// The grants the server holds.
    pub(crate) grants: Arc<LogGrants>,
    /// The server's clock.
    clock: SharedClock,
}

impl Logs {
    /// Waits until no mint is running; never for a time.
    pub(crate) async fn settle(&self) {
        let grants = Arc::clone(&self.grants);
        blocking(move || grants.settle()).await;
    }

    /// Waits until `n` mints are held by the blocked minter.
    pub(crate) async fn held(&self, n: usize) {
        let minter = Arc::clone(&self.minter);
        blocking(move || minter.wait_for_held(n)).await;
    }

    /// A client of the fake store signing with `grant`, as the agent signs its uploads.
    pub(crate) fn client(&self, grant: &LogGrant) -> LogStore<FakeStore, LogClock> {
        let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
        let credential = js_logstore::Credential {
            access_key_id: grant.access_key_id.clone(),
            secret_access_key: grant.secret_access_key.clone(),
            session_token: grant.session_token.clone(),
            expires_at: std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(grant.expires_at_ms),
        };
        let clock = LogClock(self.clock.clone());
        LogStore::new(self.minter.store().clone(), clock, endpoint, credential)
    }
}

/// The store under a server's job tables, and the tables.
type Tables = (Arc<Interfere<Fake>>, Arc<ServerJobs>);

/// What a [`logging_rig`] gives a server beyond [`rig_on`]'s: the grace after which a silent
/// host is lost (the pool's own when `None`), the grants it mints with, the job tables of a
/// server that ran before (fresh ones when `None`), and the clock of fresh tables (the rig's
/// clock when `None`).
#[derive(Default)]
struct Extra {
    grace_ms: Option<u64>,
    logs: Option<Arc<LogGrants>>,
    tables: Option<Tables>,
    store_clock: Option<Arc<StoreClock>>,
    whole: Option<WholePool>,
}

/// What makes a rig's pool a whole-node pool of hosts without GPUs ([`whole_rig`]): its
/// settings, and the longest wall time its scheduler trusts for backfill, when it backfills.
#[derive(Clone)]
struct WholePool {
    rules: WholeNodeConfig,
    backfill: Option<std::time::Duration>,
}

/// The silence after which a host of a [`whole_rig`] is lost.
pub(crate) const WHOLE_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// A server named `name` for the tests of whole-host jobs: it holds the job tables and places
/// with `js-sched`, its registry, server and store read one manual clock (at [`START_MS`]
/// until the test moves it), and its pool is a whole-node pool of up to four hosts with the
/// settings `rules`, whose hosts have no GPU and are lost after [`WHOLE_GRACE`] of silence. A
/// scheduler that backfills (a marked host takes work that ends before it is free) trusts a
/// wall time up to `backfill`, when given.
pub(crate) fn whole_rig(
    name: &str,
    rules: WholeNodeConfig,
    backfill: Option<std::time::Duration>,
) -> Rig {
    whole_over(name, (rules, backfill), None, START_MS)
}

/// [`whole_rig`] over the job tables of `before` (a server that ran on them) when given, on
/// its clock moved to `now_ms`: a server started again, with a new registry and a new
/// scheduler, whose hosts have not said hello.
pub(crate) fn whole_over(
    name: &str,
    (rules, backfill): (WholeNodeConfig, Option<std::time::Duration>),
    before: Option<&Rig>,
    now_ms: u64,
) -> Rig {
    let clock = before.map_or_else(Arc::default, |rig| Arc::clone(&rig.clock));
    let since = js_state::Clock::now(&*clock).as_duration();
    let target = std::time::Duration::from_millis(now_ms);
    clock.advance(target.saturating_sub(since));
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    let extra = Extra {
        grace_ms: Some(u64::try_from(WHOLE_GRACE.as_millis()).unwrap()),
        tables: before.map(|rig| (Arc::clone(&rig.store), Arc::clone(&rig.jobs))),
        whole: Some(WholePool { rules, backfill }),
        ..Extra::default()
    };
    let kept = (JobLimits::default(), Retention::DEFAULT);
    rig_on(name, (true, true), kept, (clock, shared), extra)
}

/// A server named `name`, holding the job tables when `with_jobs`, whose queued jobs are not
/// placed (its registry still places leases with `js-sched`).
pub(crate) fn rig(name: &str, with_jobs: bool) -> Rig {
    rig_of(name, with_jobs, false)
}

/// A server named `name` holding the job tables and placing with `js-sched`
/// (`--placement js-sched`), so its heartbeats run the pass over the queued jobs. Its hosts
/// report an `arch`, which the scheduler needs to describe them.
pub(crate) fn placing_rig(name: &str) -> Rig {
    rig_of(name, true, true)
}

/// A server named `name`, holding the job tables when `with_jobs` and placing with
/// `js-sched` when `placing`.
fn rig_of(name: &str, with_jobs: bool, placing: bool) -> Rig {
    let clock = ManualClock::new(Timestamp::from_millis(START_MS));
    let kept = (JobLimits::default(), Retention::DEFAULT);
    rig_on(
        name,
        (with_jobs, placing),
        kept,
        (Arc::default(), SharedClock::new(clock)),
        Extra::default(),
    )
}

/// A server named `name` holding the job tables and its jobs to `limits`, placing with
/// `js-sched` when `placing`, whose registry, server and store read one manual clock, at
/// [`START_MS`] until the test moves it ([`Rig::advance`]).
pub(crate) fn clocked_rig(name: &str, placing: bool, limits: JobLimits) -> Rig {
    clocked_rig_keeping(name, placing, (limits, Retention::DEFAULT))
}

/// [`clocked_rig`], its jobs held to `limits` and each ended job kept for `retention`.
pub(crate) fn clocked_rig_keeping(
    name: &str,
    placing: bool,
    (limits, retention): (JobLimits, Retention),
) -> Rig {
    let clock = Arc::new(StoreClock::default());
    clock.advance(std::time::Duration::from_millis(START_MS));
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    let kept = (limits, retention);
    let extra = Extra::default();
    rig_on(name, (true, placing), kept, (clock, shared), extra)
}

/// A server named `name` (`(with_jobs, placing)` as [`rig_of`]) holding its jobs to the limits
/// of `kept` and keeping each ended job for its retention, on the server clock `shared`, its
/// store on `clock`, with what `extra` adds.
fn rig_on(
    name: &str,
    (with_jobs, placing): (bool, bool),
    (limits, retention): (JobLimits, Retention),
    (clock, shared): (Arc<StoreClock>, SharedClock),
    extra: Extra,
) -> Rig {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_jobs");
    let dir = TestDir::under(root, name);
    let (class, gpus) = match extra.whole.is_some() {
        true => (ResourceClass::whole_node(), 0),
        false => (ResourceClass::gpu_host(), 1),
    };
    let gpu_class = (gpus > 0).then(|| "nvidia-a100".to_owned());
    let resources = Resources::new(8000, 17179869184, gpus, gpu_class).unwrap();
    let arch = ("arch".to_owned(), "x86_64".to_owned());
    let shape = Shape::new(resources, [arch].into_iter().collect());
    let pool = PoolConfig::new(ProviderId::new("burst").unwrap(), class, shape, 4);
    let pool = match &extra.whole {
        Some(whole) => pool.with_whole_node(whole.rules.clone()).unwrap(),
        None => pool,
    };
    let pool = match extra.grace_ms {
        Some(grace_ms) => {
            let timed = pool.with_timing(DEFAULT_HEARTBEAT_INTERVAL_MS, grace_ms);
            timed.unwrap()
        }
        None => pool,
    };
    let placer = {
        let backfill = extra.whole.as_ref().and_then(|whole| whole.backfill);
        let settings = crate::sched::engine::Settings {
            backfill: backfill.map_or_else(Backfill::default, Backfill::enforced),
            ..crate::sched::engine::Settings::default()
        };
        Arc::new(crate::sched::place::SchedPlacer::new(
            settings,
            shared.clone(),
        ))
    };
    let registry = Registry::with_sessions(
        pool,
        shared.clone(),
        SequentialSessions::new(),
        placer.clone(),
    );
    let placer = placing.then_some(placer);
    let store_clock = extra.store_clock.unwrap_or_else(|| Arc::clone(&clock));
    let fresh = || {
        let (store, jobs) = tables_on(store_clock, retention);
        (store, Arc::new(jobs))
    };
    let (store, jobs) = extra.tables.unwrap_or_else(fresh);
    let agents = Agents::over(registry, &shared, dir.path()).with_job_limits(limits);
    let agents = match with_jobs {
        true => agents.with_jobs(Arc::clone(&jobs)),
        false => agents,
    };
    let agents = match placer {
        Some(placer) => agents.with_placer(placer),
        None => agents,
    };
    let agents = match extra.logs {
        Some(logs) => agents.with_logs(logs),
        None => agents,
    };
    let who = agents.tokens.identify(&"j".repeat(40)).unwrap();
    Rig {
        agents: Arc::new(agents),
        clock,
        shared,
        jobs,
        store,
        who,
        sessions: BTreeMap::new(),
        gpus,
        _dir: dir,
    }
}

impl Rig {
    /// This rig's server trimming the job store's change feed through `feed` on each tick.
    pub(crate) fn with_feed(mut self, feed: crate::job_store::FeedTrim) -> Rig {
        let agents = Arc::try_unwrap(self.agents).ok().unwrap();
        self.agents = Arc::new(agents.with_job_feed(feed));
        self
    }

    /// This rig's server keeping its join tokens and host bindings in `store`, as a server with
    /// a store setting does.
    pub(crate) fn with_kept(mut self, store: Arc<dyn Store>) -> Rig {
        let agents = Arc::try_unwrap(self.agents).ok().unwrap();
        self.agents = Arc::new(agents.kept_in(store));
        self
    }

    /// Registers `host` past the route, as an agent that reports the `per-job` capability and
    /// `host-jobs` as `value`.
    pub(crate) fn register_host_jobs(&mut self, host: &str, value: &str) {
        let capabilities = json!({
            "per-job": "true",
            "host-jobs": value,
            "arch": "x86_64",
        });
        self.register_with(host, capabilities);
    }

    /// Says hello for `host` through the route, as an agent that reports the `per-job`
    /// capability and, when `uploads`, the `log-upload` capability.
    pub(crate) async fn join(&mut self, host: &str, uploads: bool) {
        let mut capabilities = json!({ "per-job": "true", "arch": "x86_64" });
        if uploads {
            capabilities["log-upload"] = json!("true");
        }
        let hello = json!({
            "host_id": host,
            "pool": "burst",
            "agent_version": "0.1.0",
            "shape": self.shape(),
            "capabilities": capabilities,
            "phase": "serving",
        });
        let body = Bytes::from(serde_json::to_vec(&hello).unwrap());
        let welcome = serve_hello(&self.agents, self.who.clone(), None, Ok(body), &|| ()).await;
        let Json(welcome) = welcome.unwrap();
        self.sessions
            .insert(host.to_owned(), (welcome.epoch, welcome.session));
    }

    /// Registers `host` past the route, as [`Rig::register`] does, as an agent that reports
    /// the `per-job` and `log-upload` capabilities: a host whose hello the registry holds and
    /// whose hello the log side never saw.
    pub(crate) fn register_uploading(&mut self, host: &str) {
        let capabilities = json!({ "per-job": "true", "log-upload": "true", "arch": "x86_64" });
        self.register_with(host, capabilities);
    }

    /// The shape the rig's hosts announce: eight cores, 16 GiB and the pool's GPUs.
    fn shape(&self) -> serde_json::Value {
        let mut shape = json!({
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": self.gpus,
        });
        if self.gpus > 0 {
            shape["gpu_class"] = json!("nvidia-a100");
        }
        shape
    }

    /// The directory the server's files are kept in.
    pub(crate) fn dir(&self) -> &Path {
        self._dir.path()
    }

    /// Registers `host`, whose agent reports the `per-job` capability when `per_job`.
    pub(crate) fn register(&mut self, host: &str, per_job: bool) {
        let capabilities = match per_job {
            true => json!({ "per-job": "true", "arch": "x86_64" }),
            false => json!({ "arch": "x86_64" }),
        };
        self.register_with(host, capabilities);
    }

    /// Registers `host`, whose agent reports `capabilities`, past the route.
    fn register_with(&mut self, host: &str, capabilities: serde_json::Value) {
        let hello: Hello = serde_json::from_value(json!({
            "host_id": host,
            "pool": "burst",
            "agent_version": "0.1.0",
            "shape": self.shape(),
            "capabilities": capabilities,
            "phase": "serving",
        }))
        .unwrap();
        let welcome = self.agents.registry.hello(&hello, &self.who).unwrap();
        self.sessions
            .insert(host.to_owned(), (welcome.epoch, welcome.session));
    }

    /// The heartbeat of `agent`'s host, answered by the route.
    pub(crate) async fn beat(&self, agent: &FakeAgent) -> Result<HeartbeatReply, Refused> {
        self.beat_reporting(agent, None).await
    }

    /// The heartbeat of `agent`'s host reporting `taint`, answered by the route.
    pub(crate) async fn beat_reporting(
        &self,
        agent: &FakeAgent,
        taint: Option<Taint>,
    ) -> Result<HeartbeatReply, Refused> {
        let (epoch, session) = self.sessions[&agent.host].clone();
        let sent = Heartbeat {
            host_id: agent.host.clone(),
            epoch,
            session,
            phase: agent.phase,
            held: Vec::new(),
            results: Vec::new(),
            taint,
            exclusive_ready: agent.ready.clone(),
            exclusive_refused: agent.refused.clone(),
            load: None,
            disk_gib: None,
            jobs: agent.listing(),
            retired_jobs: agent.retired.iter().cloned().collect(),
            log_grant_state: agent.log_state(),
        };
        let body = Bytes::from(serde_json::to_vec(&sent).unwrap());
        let agents = State(Arc::clone(&self.agents));
        let answer = super::heartbeat(agents, Extension(self.who.clone()), None, Ok(body));
        answer.await.map(|Json(reply)| reply)
    }

    /// Records the job `id` and places it on `host` with [`BOOKED`].
    pub(crate) fn place(&self, id: &str, host: &str) -> JobId {
        placed(&self.jobs, id, host, spec(), BOOKED).0.id
    }

    /// The job `id` as the store holds it.
    pub(crate) fn job(&self, id: &JobId) -> JobRecord {
        self.jobs.get_job(id).unwrap().unwrap()
    }

    /// The allocation of the job `id` as the store holds it.
    pub(crate) fn allocation(&self, id: &JobId) -> AllocationRecord {
        self.jobs.get_allocation(id).unwrap().unwrap()
    }

    /// The state of the job `id` and the step of its allocation.
    pub(crate) fn states(&self, id: &JobId) -> (JobState, AllocationStep) {
        (self.job(id).state, self.allocation(id).step)
    }

    /// How many transactions the store has committed.
    pub(crate) fn commits(&self) -> u64 {
        self.store.commit_stats().txns
    }

    /// Moves the rig's clock by `by`.
    pub(crate) fn advance(&self, by: std::time::Duration) {
        self.clock.advance(by);
    }

    /// The job half of one tick of the server, in its order ([`crate::api::Shared::tick`]):
    /// the job pass, the registry's advance, and the pass over the queued jobs.
    pub(crate) fn tick(&self) {
        self.agents.job_pass();
        self.agents.registry.advance();
        self.agents.place_jobs();
    }
}

/// A job a [`FakeAgent`] holds.
struct Held {
    allocation_id: String,
    state: HeldState,
    result: Option<JobResult>,
}

/// An agent that speaks the per-job messages, as the agent does: a start is taken once per
/// job id and held as `starting`; a stop of a held job ends it `killed`; a stop of a job it
/// does not hold retires the id, listed until the server acknowledges it; an acknowledged
/// result is dropped with its container at once, and the id is fenced, so a start that
/// arrives later is not run. It makes a container only when told to run a job.
///
/// A reply that names an allocation (`exclusive`) makes it prepare: its phase is `preparing`
/// until a test has it report the host ready ([`FakeAgent::report_ready`]) or refused
/// ([`FakeAgent::refuse`]), and the report is repeated in every heartbeat until a reply stops
/// naming the allocation; the agent is then `restoring` until the test has it serve again
/// ([`FakeAgent::serve_again`]).
///
/// An agent that uploads ([`FakeAgent::uploading`]) reports in each heartbeat the expiry of
/// the log credential it holds and whether it asks for a new one, takes the `log_grant` of a
/// reply it takes, and starts out holding none and asking.
pub(crate) struct FakeAgent {
    pub(crate) host: String,
    held: BTreeMap<String, Held>,
    retired: BTreeSet<String>,
    fenced: BTreeSet<String>,
    /// The jobs it made a container for, in order.
    pub(crate) launched: Vec<String>,
    /// Whether it reports `log_grant_state`.
    uploads: bool,
    /// The log credential it holds.
    pub(crate) grant: Option<LogGrant>,
    /// Whether it asks for a new credential.
    pub(crate) stale: bool,
    /// The phase its heartbeats report.
    pub(crate) phase: Phase,
    /// The allocation the last reply named for it to prepare for or keep.
    pub(crate) exclusive: Option<String>,
    /// The allocation it reports ready.
    pub(crate) ready: Option<String>,
    /// The refusal it reports.
    pub(crate) refused: Option<ExclusiveRefusal>,
}

impl FakeAgent {
    /// The agent of `host`, holding nothing.
    pub(crate) fn new(host: &str) -> Self {
        Self {
            host: host.to_owned(),
            held: BTreeMap::new(),
            retired: BTreeSet::new(),
            fenced: BTreeSet::new(),
            launched: Vec::new(),
            uploads: false,
            grant: None,
            stale: false,
            phase: Phase::Serving,
            exclusive: None,
            ready: None,
            refused: None,
        }
    }

    /// The agent of `host` that uploads live logs: it holds no credential yet and asks for
    /// one, as a freshly started agent does.
    pub(crate) fn uploading(host: &str) -> Self {
        Self {
            uploads: true,
            stale: true,
            ..Self::new(host)
        }
    }

    /// What its next heartbeat reports of its log credential: nothing for an agent that does
    /// not upload.
    pub(crate) fn log_state(&self) -> Option<LogGrantState> {
        self.uploads.then(|| LogGrantState {
            stale: self.stale,
            until_ms: self.grant.as_ref().map(|grant| grant.expires_at_ms),
        })
    }

    /// The job listing of its next heartbeat.
    pub(crate) fn listing(&self) -> Vec<HeldJob> {
        let listed = self.held.iter().map(|(job_id, held)| HeldJob {
            job_id: job_id.clone(),
            state: held.state,
            result: held.result.clone(),
        });
        listed.collect()
    }

    /// Whether it holds the job `id`.
    pub(crate) fn holds(&self, id: &str) -> bool {
        self.held.contains_key(id)
    }

    /// Reports the host ready for the allocation the last reply named, and serving.
    pub(crate) fn report_ready(&mut self) {
        assert!(self.exclusive.is_some(), "no reply named an allocation");
        self.ready.clone_from(&self.exclusive);
        self.phase = Phase::Serving;
    }

    /// Refuses the allocation the last reply named with `code`, still preparing.
    pub(crate) fn refuse(&mut self, code: RefusalReason) {
        let lease_id = self
            .exclusive
            .clone()
            .expect("no reply named an allocation");
        self.refused = Some(ExclusiveRefusal {
            lease_id,
            code,
            detail: String::new(),
        });
        self.phase = Phase::Preparing;
    }

    /// Serves again, as an agent does once it has restored the host.
    pub(crate) fn serve_again(&mut self) {
        self.phase = Phase::Serving;
    }

    /// Acts on `reply`: its starts, its stops and its acknowledgements, and the allocation it
    /// names for the host to prepare for (the phase `preparing`) or no longer names (the phase
    /// `restoring`, with what it reported dropped).
    pub(crate) fn take(&mut self, reply: &HeartbeatReply) {
        match (reply.exclusive.clone(), self.exclusive.clone()) {
            (Some(named), kept) if kept.as_ref() != Some(&named) => {
                self.exclusive = Some(named);
                self.phase = Phase::Preparing;
            }
            (None, Some(_)) => {
                self.exclusive = None;
                self.ready = None;
                self.refused = None;
                self.phase = Phase::Restoring;
            }
            _ => {}
        }
        for spec in starts(reply) {
            if !self.fenced.contains(&spec.job_id) {
                self.held.entry(spec.job_id.clone()).or_insert(Held {
                    allocation_id: spec.allocation_id.clone(),
                    state: HeldState::Starting,
                    result: None,
                });
            }
        }
        for job_id in stops(reply) {
            match self.held.get_mut(&job_id) {
                Some(held) => {
                    held.state = HeldState::ExitedUnacked;
                    let killed = result(&job_id, &held.allocation_id, Outcome::Killed, None);
                    held.result.get_or_insert(killed);
                }
                None => {
                    self.fenced.insert(job_id.clone());
                    self.retired.insert(job_id);
                }
            }
        }
        for ack in &reply.job_acks {
            self.held.remove(&ack.job_id);
            self.fenced.insert(ack.job_id.clone());
        }
        for id in &reply.retired_acks {
            self.retired.remove(id);
        }
        if let Some(grant) = &reply.log_grant {
            self.grant = Some(grant.clone());
            self.stale = false;
        }
    }

    /// Lists `id` as held under `allocation_id` in `state`, whatever the server said: a job
    /// it kept across a restart or a partition.
    pub(crate) fn hold(&mut self, id: &str, allocation_id: &str, state: HeldState) {
        let held = Held {
            allocation_id: allocation_id.to_owned(),
            state,
            result: None,
        };
        self.held.insert(id.to_owned(), held);
    }

    /// Makes the container of the held job `id`: it runs.
    pub(crate) fn run(&mut self, id: &str) {
        self.held.get_mut(id).unwrap().state = HeldState::Running;
        self.launched.push(id.to_owned());
    }

    /// The held job `id` ends with `outcome` and `exit_code`.
    pub(crate) fn end(&mut self, id: &str, outcome: Outcome, exit_code: Option<i32>) {
        let held = self.held.get_mut(id).unwrap();
        held.state = HeldState::ExitedUnacked;
        held.result = Some(result(id, &held.allocation_id, outcome, exit_code));
    }

    /// The held job `id` ends with a clean exit whose result reports `logs`.
    pub(crate) fn end_with_logs(&mut self, id: &str, logs: js_agent_wire::LogReport) {
        let held = self.held.get_mut(id).unwrap();
        held.state = HeldState::ExitedUnacked;
        let mut ended = result(id, &held.allocation_id, Outcome::Succeeded, Some(0));
        ended.logs = Some(logs);
        held.result = Some(ended);
    }
}

/// The result of the job `id` under `allocation_id`, with a short log tail.
pub(crate) fn result(
    id: &str,
    allocation_id: &str,
    outcome: Outcome,
    exit_code: Option<i32>,
) -> JobResult {
    JobResult {
        job_id: id.to_owned(),
        allocation_id: allocation_id.to_owned(),
        outcome,
        exit_code,
        detail: None,
        log_tail: Some(LogTail {
            stdout: "done\n".to_owned(),
            stderr: String::new(),
            truncated: false,
        }),
        logs: None,
        input_failure: None,
    }
}

/// The job starts of `reply`.
pub(crate) fn starts(reply: &HeartbeatReply) -> Vec<JobStartSpec> {
    let each = reply.commands.iter().filter_map(|command| match command {
        Command::JobStart(spec) => Some(spec.as_ref().clone()),
        _ => None,
    });
    each.collect()
}

/// The job ids `reply` says to stop.
pub(crate) fn stops(reply: &HeartbeatReply) -> Vec<String> {
    let each = reply.commands.iter().filter_map(|command| match command {
        Command::JobStop { job_id } => Some(job_id.clone()),
        _ => None,
    });
    each.collect()
}
