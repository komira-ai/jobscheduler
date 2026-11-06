//! A server with the job tables over a store that can fail a commit, hosts registered with or
//! without the `per-job` capability, and a fake of an agent that speaks the per-job messages,
//! for the tests of the heartbeat's per-job half and of the job pass. Nothing here waits on
//! time: the registry's clock and the store's clock are manual, and move only in a rig made by
//! [`clocked_rig`], where they are one clock a test moves ([`Rig::advance`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use js_agent_wire::{
    Command, HeldJob, HeldState, JobResult, JobStartSpec, LogTail, Outcome, Phase,
};
use js_jobs::interfere::Interfere;
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, Constraints, JobId, JobRecord, JobSpec, JobState,
    JobStore, Needs, NetworkPolicy, NewJob, RandomIds, Retention,
};
use js_lease::{ManualClock, Timestamp};
use js_registry::{PoolConfig, Registry, SequentialSessions};
use js_state::fake::{Fake, Flaw};
use js_state::{ManualClock as StoreClock, Store};
use js_testkit::dir::TestDir;
use js_types::{ImageDigestRef, Labels, ProviderId, ResourceClass, Resources, Shape};
use serde_json::json;

use super::*;
use crate::adapt::SharedClock;
use crate::job_expiry::JobLimits;

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
        files: js_jobs::JobFiles::default(),
        labels: Labels::default(),
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
    let jobs = JobStore::new(Arc::new(dynamic), RandomIds).unwrap();
    (store, jobs.with_retention(retention))
}

/// When a rig's clock starts: the registry's reading, in milliseconds.
pub(crate) const START_MS: u64 = 5_000_000;

/// The store's manual clock read as the server's clock: both count from the Unix epoch, so a
/// rig made by [`clocked_rig`] has one time for the registry, the server and the store.
pub(crate) struct OneClock(pub(crate) Arc<StoreClock>);

impl js_lease::Clock for OneClock {
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
    /// The store's clock; in a rig made by [`clocked_rig`], also the registry's and the
    /// server's.
    pub(crate) clock: Arc<StoreClock>,
    /// The server's clock.
    pub(crate) shared: SharedClock,
    /// The job tables the server holds (also when the server is not given them).
    pub(crate) jobs: Arc<ServerJobs>,
    /// The store under the job tables, which fails the commit it is told to.
    pub(crate) store: Arc<Interfere<Fake>>,
    who: Credential,
    sessions: BTreeMap<String, (u64, String)>,
    _dir: TestDir,
}

/// A server named `name`, holding the job tables when `with_jobs`, placing with the
/// registry's own placement (so no queued job is placed).
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
    rig_on(name, (true, placing), (limits, retention), (clock, shared))
}

/// A server named `name` (`(with_jobs, placing)` as [`rig_of`]) holding its jobs to the limits
/// of `kept` and keeping each ended job for its retention, on the server clock `shared`, its
/// store on `clock`.
fn rig_on(
    name: &str,
    (with_jobs, placing): (bool, bool),
    (limits, retention): (JobLimits, Retention),
    (clock, shared): (Arc<StoreClock>, SharedClock),
) -> Rig {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_jobs");
    let dir = TestDir::under(root, name);
    let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".to_owned())).unwrap();
    let arch = placing.then(|| ("arch".to_owned(), "x86_64".to_owned()));
    let shape = Shape::new(resources, arch.into_iter().collect());
    let pool = PoolConfig::new(
        ProviderId::new("burst").unwrap(),
        ResourceClass::gpu_host(),
        shape,
        4,
    );
    let registry = Registry::with_sessions(pool, shared.clone(), SequentialSessions::new());
    let placer = placing.then(|| {
        let settings = crate::sched::engine::Settings::default();
        Arc::new(crate::sched::place::SchedPlacer::new(
            settings,
            shared.clone(),
        ))
    });
    placer
        .iter()
        .for_each(|placer| registry.set_placer(placer.clone()));
    let (store, jobs) = tables_on(Arc::clone(&clock), retention);
    let jobs = Arc::new(jobs);
    let agents = Agents::over(registry, &shared, dir.path()).with_job_limits(limits);
    let agents = match with_jobs {
        true => agents.with_jobs(Arc::clone(&jobs)),
        false => agents,
    };
    let agents = match placer {
        Some(placer) => agents.with_placer(placer),
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
        _dir: dir,
    }
}

impl Rig {
    /// Registers `host`, whose agent reports the `per-job` capability when `per_job`.
    pub(crate) fn register(&mut self, host: &str, per_job: bool) {
        let capabilities = match per_job {
            true => json!({ "per-job": "true", "arch": "x86_64" }),
            false => json!({ "arch": "x86_64" }),
        };
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
            log_grant_state: None,
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
pub(crate) struct FakeAgent {
    pub(crate) host: String,
    held: BTreeMap<String, Held>,
    retired: BTreeSet<String>,
    fenced: BTreeSet<String>,
    /// The jobs it made a container for, in order.
    pub(crate) launched: Vec<String>,
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
        }
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
