//! The registry and the job path against the real agent lifecycle, in one process: the
//! lifecycle (over a runtime that lives in memory) takes each heartbeat reply of
//! `Registry::heartbeat`, and its held list, results and job listing make the next
//! heartbeat. A start or stop the server's job engine would add to a reply is handed to the
//! lifecycle by the test. Nothing here is scripted on the agent's side, so a command the
//! registry sends that the agent would refuse, or a held list or job listing the registry
//! misreads, fails here.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::{
    Capacity, Config, Lifecycle, MountRoots, NoChecks, ScratchRoot, StateRoot,
};
use js_agent_wire::{
    Ack, BookedResources, Command, Heartbeat, HeartbeatReply, HeldState, Isolation, JobResultAck,
    JobSpec, JobStartSpec, Limits, Outcome, Phase,
};
use js_conformance::agent::memory::{Memory as Fake, Time};
use js_registry::{Credential, PoolConfig, Registry};
use js_sched_placer::engine::Settings;
use js_sched_placer::place::SchedPlacer;
use js_testkit::dir::TestDir;
use js_types::{
    Admission, ClassId, ManualClock, ProviderId, ResourceClass, Resources, Shape, Timestamp,
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
        BTreeMap::from([
            ("os".to_string(), "macos".to_string()),
            ("arch".to_string(), "arm64".to_string()),
        ]),
    )
}

fn registry() -> Registry {
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, class(), shape(), 2);
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let placer = SchedPlacer::new(Settings::default(), clock.clone());
    Registry::new(config, clock, Arc::new(placer))
}

fn job(run: &str) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: "1-1".into(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
        network: Default::default(),
        ports: Vec::new(),
        run: None,
        os: None,
        arch: None,
    }
}

/// One agent: the real lifecycle, and the session it holds with a registry.
struct Agent {
    life: Lifecycle,
    fake: Fake,
    epoch: u64,
    session: String,
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
                disk_bytes: 0,
                gpus: Default::default(),
            },
            allow_host_jobs: false,
            mounts: MountRoots::new(&[]).unwrap(),
            scratch: ScratchRoot::open(dir.path()).unwrap(),
            files: None,
            state: StateRoot::open(state.path()).unwrap(),
            stop_grace: Duration::from_secs(1),
            bundle: None,
            quota: None,
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
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs: self.life.held_jobs(),
            retired_jobs: self.life.retired_jobs(),
            log_grant_state: None,
        };
        let reply = registry.heartbeat(&message, &credential()).unwrap();
        self.take(&reply);
        reply
    }

    /// Hands `reply` to the lifecycle and lets it take a step.
    fn take(&mut self, reply: &HeartbeatReply) {
        self.life.apply(reply);
        self.life.step();
    }

    /// Hands the lifecycle a reply that carries `commands` alone, as a server whose job
    /// engine added them would.
    fn command(&mut self, commands: Vec<Command>) {
        let reply = HeartbeatReply {
            commands,
            ..HeartbeatReply::default()
        };
        self.take(&reply);
    }

    /// Hands the lifecycle a reply that carries the verdict `ack` on the result of `job`.
    fn acknowledge(&mut self, job: &str, ack: Ack) {
        let job_acks = vec![JobResultAck {
            job_id: job.to_string(),
            ack,
        }];
        let reply = HeartbeatReply {
            job_acks,
            ..HeartbeatReply::default()
        };
        self.take(&reply);
    }
}

fn allocation() -> String {
    "0a".repeat(16)
}

/// The start the server's job engine adds to a reply for `job`, booking `cpu_millis`.
fn job_start(job: &str, cpu_millis: u64) -> Command {
    Command::JobStart(Box::new(JobStartSpec {
        job_id: job.into(),
        allocation_id: allocation(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        command: Vec::new(),
        run: None,
        os: None,
        arch: None,
        env: BTreeMap::new(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
    }))
}

/// What the agent lists in `jobs`, as `(job, state, outcome)`.
fn listed(agent: &Agent) -> Vec<(String, HeldState, Option<Outcome>)> {
    let held = agent.life.held_jobs();
    let word = |job: &js_agent_wire::HeldJob| {
        let outcome = job.result.as_ref().map(|result| result.outcome);
        (job.job_id.clone(), job.state, outcome)
    };
    held.iter().map(word).collect()
}

/// Asserts a job's whole life against the real agent and a registry that holds no run:
/// the start the job engine adds is one the agent accepts and runs; the job is listed
/// running in `jobs` and the registry's reply to that heartbeat carries no command (the
/// registry does not read a listed job as a run it must cancel) and leaves the host not
/// releasing; when the container exits the result is listed with the job, never in `held`
/// or `results`, and stays listed until the job's acknowledgement drops it; and the booking
/// returns, so a job that takes the whole host then starts. It catches a start the agent
/// refuses, a job listing the registry answers with a cancel or reads as a held run, a
/// result the acknowledgement does not drop, and a booking that never returns.
#[test]
fn a_job_starts_exits_and_is_acknowledged() {
    let registry = registry();
    let mut agent = Agent::join(&registry, "job-life");
    agent.command(vec![job_start("job-1", 1000)]);
    assert_eq!(agent.fake.created(), ["job-1"]);

    let reply = agent.beat(&registry);
    assert!(reply.commands.is_empty());
    assert!(reply.result_acks.is_empty());
    assert_eq!(
        listed(&agent),
        [("job-1".to_string(), HeldState::Running, None)]
    );
    assert!(agent.life.held().is_empty());
    assert!(!registry.hosts()[0].releasing);

    agent.fake.exit("job-1", 0);
    agent.life.step();
    let reply = agent.beat(&registry);
    assert!(reply.commands.is_empty());
    assert_eq!(
        listed(&agent),
        [(
            "job-1".to_string(),
            HeldState::ExitedUnacked,
            Some(Outcome::Succeeded)
        )]
    );
    assert!(agent.life.results().is_empty());

    agent.acknowledge("job-1", Ack::Accepted);
    assert!(listed(&agent).is_empty());
    assert_eq!(agent.fake.alive(), 0);
    assert!(agent.beat(&registry).commands.is_empty());

    agent.command(vec![job_start("job-2", 4000)]);
    assert_eq!(
        listed(&agent),
        [("job-2".to_string(), HeldState::Running, None)]
    );
}

/// Asserts a job stop against the real agent: the stop the job engine adds ends the
/// container, the agent lists the job killed, the registry's reply to that heartbeat still
/// carries no command, the acknowledgement drops the result, and the container is gone. It
/// catches a stop the agent ignores and a killed result the acknowledgement does not drop.
#[test]
fn a_job_stop_kills_the_job_and_its_result_is_taken() {
    let registry = registry();
    let mut agent = Agent::join(&registry, "job-stop");
    agent.command(vec![job_start("job-1", 1000)]);
    agent.beat(&registry);
    agent.command(vec![Command::JobStop {
        job_id: "job-1".into(),
    }]);

    let reply = agent.beat(&registry);
    assert!(reply.commands.is_empty());
    assert_eq!(
        listed(&agent),
        [(
            "job-1".to_string(),
            HeldState::ExitedUnacked,
            Some(Outcome::Killed)
        )]
    );
    agent.acknowledge("job-1", Ack::Accepted);
    assert!(listed(&agent).is_empty());
    assert_eq!(agent.fake.alive(), 0);
}

/// Asserts what a server that forgot a run does to the agent's work: a registry with no
/// record of the run (a restart) is told by the hello that the agent holds it, the host
/// reads releasing, the next reply cancels the run, the agent kills it and sends no result at all, and the host is quiet
/// after the agent lists nothing. The work is lost and nothing says so. It catches a cancel
/// the agent does not obey, a result that does arrive, and a host left releasing.
#[test]
fn a_cancel_of_a_run_the_server_does_not_know_kills_it_without_a_result() {
    let before = registry();
    let mut agent = Agent::join(&before, "cancel");
    agent.command(vec![Command::Start(Box::new(job("r1")))]);
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

/// Asserts the answer to the result of a run the server holds no record of, from an agent
/// that kept it across the server's restart: the run has exited, the registry cancels it
/// and answers its result `UnknownLease`, and the agent drops the result and frees the
/// run, after which the host is quiet. It catches a result the agent keeps sending after
/// that answer and a host left releasing.
#[test]
fn the_result_of_a_run_the_server_does_not_know_is_answered_unknown_and_dropped() {
    let before = registry();
    let mut agent = Agent::join(&before, "unknown-result");
    agent.command(vec![Command::Start(Box::new(job("r1")))]);
    agent.fake.exit("r1", 0);
    agent.life.step();
    assert_eq!(agent.life.results().len(), 1);

    let after = registry();
    agent.hello(&after);
    let reply = agent.beat(&after);
    assert_eq!(words(&reply), ["cancel r1"]);
    assert_eq!(reply.result_acks.len(), 1);
    assert_eq!(reply.result_acks[0].ack, Ack::UnknownLease);
    assert!(agent.life.results().is_empty());
    assert!(after.hosts()[0].releasing);

    assert!(agent.beat(&after).commands.is_empty());
    assert!(!after.hosts()[0].releasing);
    assert_eq!(agent.fake.alive(), 0);
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

fn words(reply: &HeartbeatReply) -> Vec<String> {
    let word = |command: &Command| match command {
        Command::Start(spec) => format!("start {}", spec.run_id),
        Command::Stop { run_id } => format!("stop {run_id}"),
        Command::Cancel { run_id } => format!("cancel {run_id}"),
        Command::JobStart(spec) => format!("job_start {}", spec.job_id),
        Command::JobStop { job_id } => format!("job_stop {job_id}"),
        Command::Unknown => "unknown".into(),
    };
    reply.commands.iter().map(word).collect()
}
