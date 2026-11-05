//! A server with the job tables over a store that can fail a commit, hosts registered with or
//! without the `per-job` capability, and a fake of an agent that speaks the per-job messages,
//! for the tests of the heartbeat's per-job half. Nothing here waits on time: the registry's
//! clock and the store's clock are manual and move only when a test advances them.
//!
//! A server with a log sink ([`logging_rig`]) mints its host credentials with js-logmint's
//! fake minter over js-logstore's fake store, both on the server's clock; its hosts say hello
//! through the route ([`Rig::join`]), and the fake agent can report `log-upload` and the
//! credential it holds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use js_agent_wire::{
    Command, DEFAULT_HEARTBEAT_INTERVAL_MS, HeldJob, HeldState, JobResult, JobStartSpec, LogGrant,
    LogGrantState, LogTail, Outcome, Phase,
};
use js_jobs::interfere::Interfere;
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, Constraints, JobId, JobRecord, JobSpec, JobState,
    JobStore, Needs, NetworkPolicy, NewJob, RandomIds,
};
use js_lease::{Clock as _, ManualClock, Timestamp};
use js_logmint::fake::FakeMinter;
use js_logmint::{Bucket, LogPrefix};
use js_logstore::fake::FakeStore;
use js_logstore::{Endpoint, Store as LogStore};
use js_registry::{PoolConfig, Registry, SequentialSessions};
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use js_testkit::dir::TestDir;
use js_types::{ImageDigestRef, Labels, ProviderId, ResourceClass, Resources, Shape};
use serde_json::json;

use super::*;
use crate::adapt::SharedClock;
use crate::logs::{Scope, SinkTemplate, StoreClock};

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
        image: ImageDigestRef::parse(&image()).unwrap(),
        command: vec!["run".to_owned()],
        args: vec!["--fast".to_owned()],
        env: BTreeMap::new(),
        needs: Needs::Shape(Resources::new(1500, 1 << 30, 0, None).unwrap()),
        constraints: Constraints::default(),
        wall_limit_secs: 600,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: BTreeMap::new(),
        labels: Labels::default(),
    }
}

/// Job tables of the server's own type over a fresh store that can fail a commit, so a test
/// without a server runs the same code a heartbeat does.
pub(crate) fn tables() -> (Arc<Interfere<Fake>>, ServerJobs) {
    let store = Arc::new(Interfere::new(Fake::with_flaw(Arc::default(), Flaw::None)));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = JobStore::new(Arc::new(dynamic), RandomIds).unwrap();
    (store, jobs)
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
    };
    jobs.insert_queued(new).unwrap();
    jobs.place(&id, js_types::HostId::new(host).unwrap(), booked)
        .unwrap()
}

/// The server under test and the store under it.
pub(crate) struct Rig {
    pub(crate) agents: Arc<Agents>,
    /// The job tables the server holds (also when the server is not given them).
    pub(crate) jobs: Arc<ServerJobs>,
    /// The store under the job tables, which fails the commit it is told to.
    pub(crate) store: Arc<Interfere<Fake>>,
    /// The clock of the registry and of the log credentials.
    pub(crate) clock: ManualClock,
    who: Credential,
    sessions: BTreeMap<String, (u64, String)>,
    _dir: TestDir,
}

/// When every test server starts, in milliseconds since the epoch.
pub(crate) const START_MS: u64 = 5_000_000;

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

/// A server named `name`, holding the job tables when `with_jobs`.
pub(crate) fn rig(name: &str, with_jobs: bool) -> Rig {
    build(name, with_jobs, None, None, None)
}

/// A server named `name` that holds the job tables and minting through a fake minter, whose
/// hosts are lost after `grace_ms` of silence; the fake minter and the server's grants.
pub(crate) fn logging_rig(name: &str, grace_ms: u64) -> (Rig, Logs) {
    logging_over(name, grace_ms, None, START_MS)
}

/// [`logging_rig`] over the job tables of `tables` (a store a server ran on before) when
/// given, with its clock at `now_ms`: a server rebuilt on the same store, with a new registry
/// and nothing of the log credentials held before.
pub(crate) fn logging_over(
    name: &str,
    grace_ms: u64,
    tables: Option<&Rig>,
    now_ms: u64,
) -> (Rig, Logs) {
    let clock = ManualClock::new(Timestamp::from_millis(now_ms));
    let shared = SharedClock::new(clock.clone());
    let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
    let store = FakeStore::new(endpoint.clone(), Arc::new(StoreClock(shared.clone())));
    let minter = Arc::new(FakeMinter::new(store));
    let grants = grants_over(
        endpoint,
        Arc::clone(&minter) as Arc<dyn js_logmint::Minter>,
        &shared,
    );
    let tables = tables.map(|rig| (Arc::clone(&rig.store), Arc::clone(&rig.jobs)));
    let timing = Some((clock, grace_ms));
    let rig = build(name, true, timing, Some(Arc::clone(&grants)), tables);
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
    pub(crate) fn client(&self, grant: &LogGrant) -> LogStore<FakeStore, StoreClock> {
        let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
        let credential = js_logstore::Credential {
            access_key_id: grant.access_key_id.clone(),
            secret_access_key: grant.secret_access_key.clone(),
            session_token: grant.session_token.clone(),
            expires_at: std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(grant.expires_at_ms),
        };
        let clock = StoreClock(self.clock.clone());
        LogStore::new(self.minter.store().clone(), clock, endpoint, credential)
    }
}

/// The store under a server's job tables, and the tables.
type Tables = (Arc<Interfere<Fake>>, Arc<ServerJobs>);

/// A server named `name`: with the job tables when `with_jobs` (`tables` when given, fresh
/// ones otherwise), on `timing`'s clock and grace when given (a fixed clock and the pool's
/// grace otherwise), and minting with `logs`.
fn build(
    name: &str,
    with_jobs: bool,
    timing: Option<(ManualClock, u64)>,
    logs: Option<Arc<LogGrants>>,
    tables: Option<Tables>,
) -> Rig {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_jobs");
    let dir = TestDir::under(root, name);
    let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".to_owned())).unwrap();
    let shape = Shape::new(resources, BTreeMap::new());
    let pool = PoolConfig::new(
        ProviderId::new("burst").unwrap(),
        ResourceClass::gpu_host(),
        shape,
        4,
    );
    let (clock, pool) = match timing {
        Some((clock, grace_ms)) => {
            let timed = pool.with_timing(DEFAULT_HEARTBEAT_INTERVAL_MS, grace_ms);
            (clock, timed.unwrap())
        }
        None => (ManualClock::new(Timestamp::from_millis(START_MS)), pool),
    };
    let shared = SharedClock::new(clock.clone());
    let registry = Registry::with_sessions(pool, clock.clone(), SequentialSessions::new());
    let fresh = || {
        let (store, jobs) = self::tables();
        (store, Arc::new(jobs))
    };
    let (store, jobs) = tables.unwrap_or_else(fresh);
    let agents = Agents::over(registry, &shared, dir.path());
    let agents = match with_jobs {
        true => agents.with_jobs(Arc::clone(&jobs)),
        false => agents,
    };
    let agents = match logs {
        Some(logs) => agents.with_logs(logs),
        None => agents,
    };
    let who = agents.tokens.identify(&"j".repeat(40)).unwrap();
    Rig {
        agents: Arc::new(agents),
        jobs,
        store,
        clock,
        who,
        sessions: BTreeMap::new(),
        _dir: dir,
    }
}

impl Rig {
    /// Says hello for `host` through the route, as an agent that reports the `per-job`
    /// capability and, when `uploads`, the `log-upload` capability.
    pub(crate) async fn join(&mut self, host: &str, uploads: bool) {
        let mut capabilities = json!({ "per-job": "true" });
        if uploads {
            capabilities["log-upload"] = json!("true");
        }
        let hello = json!({
            "host_id": host,
            "pool": "burst",
            "agent_version": "0.1.0",
            "shape": {
                "cpu_millis": 8000,
                "memory_bytes": 17179869184u64,
                "gpus": 1,
                "gpu_class": "nvidia-a100",
            },
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
        self.register_with(host, json!({ "per-job": "true", "log-upload": "true" }));
    }

    /// The directory the server's files are kept in.
    pub(crate) fn dir(&self) -> &Path {
        self._dir.path()
    }

    /// Moves the clock on by `millis`.
    pub(crate) fn advance(&self, millis: u64) {
        let now = self.clock.now().millis();
        self.clock.set(Timestamp::from_millis(now + millis));
    }

    /// Registers `host`, whose agent reports the `per-job` capability when `per_job`.
    pub(crate) fn register(&mut self, host: &str, per_job: bool) {
        let capabilities = match per_job {
            true => json!({ "per-job": "true" }),
            false => json!({}),
        };
        self.register_with(host, capabilities);
    }

    /// Registers `host`, whose agent reports `capabilities`, past the route.
    fn register_with(&mut self, host: &str, capabilities: serde_json::Value) {
        let hello: Hello = serde_json::from_value(json!({
            "host_id": host,
            "pool": "burst",
            "agent_version": "0.1.0",
            "shape": {
                "cpu_millis": 8000,
                "memory_bytes": 17179869184u64,
                "gpus": 1,
                "gpu_class": "nvidia-a100",
            },
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
        let (epoch, session) = self.sessions[&agent.host].clone();
        let sent = Heartbeat {
            host_id: agent.host.clone(),
            epoch,
            session,
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
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

    /// Acts on `reply`: its starts, its stops and its acknowledgements.
    pub(crate) fn take(&mut self, reply: &HeartbeatReply) {
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
