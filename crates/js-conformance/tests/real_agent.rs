//! The registry's run delivery against the real agent lifecycle, in one process: the
//! lifecycle (over a runtime that lives in memory) takes each heartbeat reply of
//! `Registry::heartbeat`, and its held list and results make the next heartbeat. Nothing
//! here is scripted on the agent's side, so a command the registry sends that the agent
//! would refuse, or a held list the registry misreads, fails here.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    Capacity, Clock, Config, Exit, Found, Lifecycle, MountRoots, NoChecks, RunHandle, RunSpec,
    Runtime, RuntimeError, ScratchRoot, StateRoot, Status,
};
use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, Isolation, JobSpec, Limits, Outcome, Phase,
};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Observed, Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{Credential, PoolConfig, Registry, RunState};
use js_testkit::dir::TestDir;
use js_types::{
    Admission, Capabilities, ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass,
    Resources, Shape,
};

mod common;
use common::hello;

fn class() -> ResourceClass {
    ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0)
}

fn shape() -> Shape {
    let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
    Shape::new(
        resources,
        BTreeMap::from([("os".to_string(), "macos".to_string())]),
    )
}

fn registry() -> Registry {
    registry_of(class())
}

fn registry_of(class: ResourceClass) -> Registry {
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, class, shape(), 2);
    Registry::new(config, ManualClock::new(Timestamp::from_millis(1_000_000)))
}

fn start_spec(sequence: u64) -> StartSpec {
    let request = Request {
        class: ClassId::new("mac-host").unwrap(),
        resources: Resources::new(1000, 1, 0, None).unwrap(),
        capabilities: Capabilities::from([("os".to_string(), "macos".to_string())]),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    StartSpec::new(LeaseId::new(1, sequence), request, &[class()]).unwrap()
}

fn job(run: &str) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
    }
}

/// A runtime whose containers exist in memory and exit when the test says so or when they
/// are stopped.
#[derive(Clone, Default)]
struct Fake(Rc<RefCell<World>>);

#[derive(Default)]
struct World {
    containers: BTreeMap<String, (String, Option<Exit>)>,
    next: u32,
    created: Vec<String>,
}

impl Fake {
    fn exit(&self, run: &str, code: i32) {
        let mut world = self.0.borrow_mut();
        let found = world.containers.values_mut().find(|(r, _)| r == run);
        found.expect("no such container").1 = Some(Exit {
            code,
            oom_killed: false,
        });
    }

    fn created(&self) -> Vec<String> {
        self.0.borrow().created.clone()
    }

    fn alive(&self) -> usize {
        self.0.borrow().containers.len()
    }
}

impl Runtime for Fake {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let mut world = self.0.borrow_mut();
        world.next += 1;
        let id = format!("c{}", world.next);
        world.created.push(spec.run_id.clone());
        world
            .containers
            .insert(id.clone(), (spec.run_id.clone(), None));
        Ok(RunHandle(id))
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        let world = self.0.borrow();
        Ok(match world.containers.get(&id.0) {
            None => Status::Gone,
            Some((_, exit)) => exit.map_or(Status::Running, Status::Exited),
        })
    }

    fn stop(&mut self, id: &RunHandle, _grace: Duration) -> Result<(), RuntimeError> {
        let mut world = self.0.borrow_mut();
        if let Some((_, exit)) = world.containers.get_mut(&id.0) {
            exit.get_or_insert(Exit {
                code: 137,
                oom_killed: false,
            });
        }
        Ok(())
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.0.borrow_mut().containers.remove(&id.0);
        Ok(())
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        Ok(Vec::new())
    }
}

struct Time(Rc<Cell<u64>>);

impl Clock for Time {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// One agent: the real lifecycle, and the session it holds with a registry.
struct Agent {
    life: Lifecycle,
    fake: Fake,
    epoch: u64,
    session: String,
    /// The lease the last reply told the agent to prepare for; the next heartbeat reports
    /// it ready (the real agent has no preparation step yet, so a prepared host is one
    /// that has seen the word).
    prepared: Option<String>,
    /// The scratch and state roots, removed after the lifecycle that uses them is gone.
    _dirs: (TestDir, TestDir),
}

fn credential() -> Credential {
    Credential::of_token("join-token")
}

impl Agent {
    /// An agent for host `a` that has said hello to `registry`.
    fn join(registry: &Registry, name: &str) -> Self {
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let dir = TestDir::under(&target, &format!("real-agent-{name}"));
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state = TestDir::under(&target, &format!("real-agent-{name}-state"));
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config {
            capacity: Capacity {
                cpu_millis: 4000,
                memory_bytes: 8 << 30,
                gpus: Default::default(),
            },
            allow_host_jobs: false,
            mounts: MountRoots::new(&[]).unwrap(),
            scratch: ScratchRoot::open(dir.path()).unwrap(),
            state: StateRoot::open(state.path()).unwrap(),
            stop_grace: Duration::from_secs(1),
        };
        let fake = Fake::default();
        let (life, _) = Lifecycle::adopt(
            config,
            Box::new(fake.clone()),
            Box::new(NoChecks),
            Box::new(Time(Rc::default())),
        )
        .unwrap();
        let mut agent = Self {
            life,
            fake,
            epoch: 0,
            session: String::new(),
            prepared: None,
            _dirs: (dir, state),
        };
        agent.hello(registry);
        agent
    }

    /// Says hello to `registry` listing what the lifecycle holds.
    fn hello(&mut self, registry: &Registry) {
        let mut message = hello("a", &shape());
        message.held = self.life.held();
        let welcome = registry.hello(&message, &credential()).unwrap();
        self.epoch = welcome.epoch;
        self.session = welcome.session;
    }

    /// One heartbeat: the lifecycle's held runs and results go up, the reply is applied
    /// and the lifecycle takes a step. Returns the reply.
    fn beat(&mut self, registry: &Registry) -> HeartbeatReply {
        let message = Heartbeat {
            host_id: "a".into(),
            epoch: self.epoch,
            session: self.session.clone(),
            phase: Phase::Serving,
            held: self.life.held(),
            results: self.life.results(),
            taint: None,
            exclusive_ready: self.prepared.take(),
            exclusive_refused: None,
            load: None,
        };
        let reply = registry.heartbeat(&message, &credential()).unwrap();
        self.prepared = reply.exclusive.clone();
        self.life.apply(&reply);
        self.life.step();
        reply
    }
}

fn words(reply: &HeartbeatReply) -> Vec<String> {
    let word = |command: &Command| match command {
        Command::Start(spec) => format!("start {}", spec.run_id),
        Command::Stop { run_id } => format!("stop {run_id}"),
        Command::Cancel { run_id } => format!("cancel {run_id}"),
        Command::Unknown => "unknown".into(),
    };
    reply.commands.iter().map(word).collect()
}

fn leased(registry: &Registry) -> LeaseId {
    let mut provider = registry.provider();
    assert!(matches!(
        provider.start(start_spec(1)),
        StartAck::Started { .. }
    ));
    LeaseId::new(1, 1)
}

fn state(registry: &Registry, lease: &LeaseId, run: &str) -> RunState {
    let runs = registry.runs_of(lease);
    runs.into_iter().find(|v| v.run_id == run).unwrap().state
}

fn host() -> HostId {
    HostId::new("a").unwrap()
}

/// Asserts a run's whole life against the real agent: the start the registry sends is one
/// the agent accepts and runs; the agent's held list moves the run through queued and
/// running; when the container exits the agent's result is accepted and shown, the
/// acknowledgement makes the agent drop it, and when the agent has freed the run the
/// registry reads it ended and gives the budget back. It catches a start the agent
/// refuses, a held list or acknowledgement the two sides read differently, and a budget
/// that never returns.
#[test]
fn a_run_starts_exits_and_is_acknowledged() {
    let registry = registry();
    let mut agent = Agent::join(&registry, "life");
    let lease = leased(&registry);
    registry.queue_run(&host(), &lease, job("r1")).unwrap();

    assert_eq!(words(&agent.beat(&registry)), ["start r1"]);
    assert_eq!(agent.fake.created(), ["r1"]);
    assert_eq!(state(&registry, &lease, "r1"), RunState::Queued);
    assert!(words(&agent.beat(&registry)).is_empty());
    assert_eq!(state(&registry, &lease, "r1"), RunState::Running);

    agent.fake.exit("r1", 0);
    agent.life.step();
    let reply = agent.beat(&registry);
    assert_eq!(reply.result_acks.len(), 1);
    assert_eq!(reply.result_acks[0].ack, Ack::Accepted);
    let views = registry.runs_of(&lease);
    assert_eq!(views[0].state, RunState::Exited);
    assert_eq!(
        views[0].result.as_ref().unwrap().outcome,
        Outcome::Succeeded
    );
    assert!(agent.life.results().is_empty());

    assert!(words(&agent.beat(&registry)).is_empty());
    assert_eq!(state(&registry, &lease, "r1"), RunState::Ended);
    assert_eq!(agent.fake.alive(), 0);
    let mut big = job("r2");
    big.limits.cpu_millis = 4000;
    assert!(registry.queue_run(&host(), &lease, big).is_ok());
}

/// Asserts a stop against the real agent: the stop the registry sends ends the container,
/// the agent reports the run killed, the result is accepted, and the run ends when the
/// agent has freed it. It catches a stop the agent ignores or a killed result the
/// registry refuses.
#[test]
fn a_stop_kills_the_run_and_its_result_is_taken() {
    let registry = registry();
    let mut agent = Agent::join(&registry, "stop");
    let lease = leased(&registry);
    registry.queue_run(&host(), &lease, job("r1")).unwrap();
    agent.beat(&registry);
    agent.beat(&registry);
    registry.stop_run(&lease, "r1").unwrap();

    assert_eq!(words(&agent.beat(&registry)), ["stop r1"]);
    let reply = agent.beat(&registry);
    assert_eq!(reply.result_acks[0].ack, Ack::Accepted);
    let views = registry.runs_of(&lease);
    assert_eq!(views[0].state, RunState::Exited);
    assert_eq!(views[0].result.as_ref().unwrap().outcome, Outcome::Killed);
    agent.beat(&registry);
    assert_eq!(state(&registry, &lease, "r1"), RunState::Ended);
    assert_eq!(agent.fake.alive(), 0);
}

/// Asserts what a server that forgot a run does to the agent's work: a registry with no
/// record of the run (a restart) is told by the hello that the agent holds it, the next
/// reply cancels it, the agent kills it and sends no result at all, and the host is quiet
/// after the agent lists nothing. The work is lost and nothing says so. It catches a
/// cancel the agent does not obey, a result that does arrive, and a host left releasing.
#[test]
fn a_cancel_of_a_forgotten_run_kills_it_without_a_result() {
    let before = registry();
    let mut agent = Agent::join(&before, "cancel");
    let lease = leased(&before);
    before.queue_run(&host(), &lease, job("r1")).unwrap();
    agent.beat(&before);
    agent.beat(&before);
    assert_eq!(agent.fake.alive(), 1);

    let after = registry();
    agent.hello(&after);
    assert!(after.hosts()[0].releasing);
    assert_eq!(words(&agent.beat(&after)), ["cancel r1"]);
    assert!(agent.life.results().is_empty());
    assert_eq!(agent.fake.alive(), 0);
    let mut sent_a_result = false;
    for _ in 0..3 {
        let message_results = agent.life.results();
        sent_a_result |= !message_results.is_empty();
        agent.beat(&after);
    }
    assert!(!sent_a_result);
    assert!(!after.hosts()[0].releasing);
}

/// Asserts the end of a lease against the real agent: the booking is free at once, the
/// host reads releasing and takes no new lease while the agent lists the run, the stop
/// reaches the agent and kills the run, its late result is answered stale and dropped, and
/// once the agent lists nothing the host is quiet and takes the next lease. It catches a
/// host handed over under a running job.
#[test]
fn a_lease_end_stops_the_run_then_the_host_is_quiet() {
    let registry = registry();
    let mut agent = Agent::join(&registry, "lease-end");
    let lease = leased(&registry);
    registry.queue_run(&host(), &lease, job("r1")).unwrap();
    agent.beat(&registry);
    agent.beat(&registry);

    let mut provider = registry.provider();
    provider.stop(&lease);
    assert!(registry.hosts()[0].releasing);
    assert_eq!(
        provider.start(start_spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(words(&agent.beat(&registry)), ["stop r1"]);
    let reply = agent.beat(&registry);
    assert_eq!(reply.result_acks[0].ack, Ack::Stale);
    assert!(agent.life.results().is_empty());
    assert!(registry.hosts()[0].releasing);
    assert!(agent.beat(&registry).commands.is_empty());
    assert!(!registry.hosts()[0].releasing);
    assert!(matches!(
        provider.start(start_spec(2)),
        StartAck::Started { .. }
    ));
    assert_eq!(agent.fake.alive(), 0);
}

/// Asserts the exclusive gate against the real agent. A whole-node lease is answered
/// Unknown and reads Unknown while pending; the first heartbeat reply names it in
/// `exclusive`, and a run queued then is refused as not ready and sent to nobody; the
/// agent's next heartbeat reports it ready, the lease reads Present, and the same run
/// is queued, started and acknowledged as for any lease. It catches a start the
/// registry answers `Started` before the host is prepared, a reply that does not tell
/// the agent what to prepare for, and a ready report the registry does not read.
#[test]
fn a_whole_node_lease_is_usable_after_the_agent_reports_it_ready() {
    let registry = registry_of(ResourceClass::whole_node());
    let mut agent = Agent::join(&registry, "whole");
    let whole = ResourceClass::whole_node();
    let request = Request {
        class: whole.id().clone(),
        ..start_spec(1).request().clone()
    };
    let lease = LeaseId::new(1, 1);
    let mut provider = registry.provider();
    let spec = StartSpec::new(lease, request, &[whole]).unwrap();
    assert!(matches!(provider.start(spec), StartAck::Unknown));
    assert_eq!(provider.observe(&lease), Observed::Unknown);

    let told = agent.beat(&registry);
    assert_eq!(told.exclusive, Some(lease.to_string()));
    assert!(registry.queue_run(&host(), &lease, job("r1")).is_err());
    agent.beat(&registry);
    assert!(matches!(provider.observe(&lease), Observed::Present { .. }));

    registry.queue_run(&host(), &lease, job("r1")).unwrap();
    assert_eq!(words(&agent.beat(&registry)), ["start r1"]);
}

/// Asserts that two agents joined under one name (each to a registry of its own, since a
/// host joins a registry once) get two scratch roots and two state roots, each named with
/// this process's id: the directories under the target directory whose names carry
/// `-<pid>-` and the agents' name are four. It catches a root named by
/// the test alone, which a second run of this binary (a coverage run beside a test run)
/// or a second agent of the name would remove under the first.
#[test]
fn agents_of_one_name_get_roots_of_their_own() {
    let _first = Agent::join(&registry(), "same-root");
    let _second = Agent::join(&registry(), "same-root");
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let mine = format!("-{}-", std::process::id());
    let roots = fs::read_dir(target)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("real-agent-same-root") && name.contains(&mine))
        .count();
    assert_eq!(roots, 4);
}
