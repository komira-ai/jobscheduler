//! The host-agent suite against the real js-agent lifecycle. The adapter drives
//! `Lifecycle` over a runtime that lives in memory and a clock the suite moves, and stamps
//! the agent's `Contact` where the session would: a message the harness delivers is a
//! reply the session accepted, and `contact` is an answered heartbeat. A `start` is a run
//! start (it carries the validity bound), a `start_fenced` a job start whose fence is the
//! job with its allocation. The fence window is the agent's default silence bound
//! (`--silence-stop`).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    Capacity, Clock, Config, Contact, DEFAULT_SILENCE_STOP, Event, Exit, Found, Lifecycle,
    MountRoots, NoChecks, RunHandle, RunSpec, Runtime, RuntimeError, ScratchRoot, Skip, StateRoot,
    Status, detail,
};
use js_agent_wire::{
    Ack, BookedResources, Command, HeartbeatReply, HeldState, Isolation, JobResultAck, JobSpec,
    JobStartSpec, Limits, ResultAck,
};
use js_conformance::Verdict;
use js_conformance::agent::{self, AgentHarness, Delivery, Observed, Scenario};
use js_testkit::dir::TestDir;

const IMAGE: &str =
    "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Where the suite's clock starts on the lifecycle's, so that a start may be backdated by
/// any wait the scenarios name: the suite reads the clock from 0.
const BASE: u64 = 1 << 40;

/// A runtime whose containers live in memory, exit when the suite ends a job and when they
/// are stopped, and count what was created.
#[derive(Clone, Default)]
struct Fake(Rc<RefCell<World>>);

#[derive(Default)]
struct World {
    containers: BTreeMap<String, (String, Option<Exit>)>,
    next: u32,
    created: Vec<String>,
}

impl Fake {
    fn exit(&self, run: &str) {
        let mut world = self.0.borrow_mut();
        let found = world.containers.values_mut().find(|(r, _)| r == run);
        if let Some((_, exit)) = found {
            exit.get_or_insert(Exit {
                code: 0,
                oom_killed: false,
            });
        }
    }

    fn launches(&self) -> BTreeMap<String, u32> {
        let mut launches = BTreeMap::new();
        for run in &self.0.borrow().created {
            *launches.entry(run.clone()).or_insert(0) += 1;
        }
        launches
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

/// A private directory for the lifecycle's roots.
fn private(name: &str) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// One js-agent lifecycle behind the suite's operations.
struct JsAgent {
    life: Lifecycle,
    fake: Fake,
    clock: Rc<Cell<u64>>,
    contact: Contact,
    connected: bool,
    /// The names started by a job start: their results and acknowledgements are the job's.
    jobs: BTreeSet<String>,
    never_started: BTreeSet<String>,
    sent: Vec<String>,
    /// The scratch and state roots, removed after the lifecycle that uses them is gone.
    _dirs: (TestDir, TestDir),
}

impl JsAgent {
    fn new() -> Self {
        let (scratch, state) = (private("js-agent-suite"), private("js-agent-suite-state"));
        let config = Config {
            capacity: Capacity {
                cpu_millis: 4000,
                memory_bytes: 8 << 30,
                disk_bytes: 0,
                gpus: Default::default(),
            },
            allow_host_jobs: false,
            mounts: MountRoots::new(&[]).unwrap(),
            scratch: ScratchRoot::open(scratch.path()).unwrap(),
            files: None,
            state: StateRoot::open(state.path()).unwrap(),
            stop_grace: Duration::from_secs(1),
            bundle: None,
            quota: None,
        };
        let (fake, clock, contact) = (
            Fake::default(),
            Rc::new(Cell::new(BASE)),
            Contact::default(),
        );
        let (mut life, _) = Lifecycle::adopt(
            config,
            Box::new(fake.clone()),
            Box::new(NoChecks),
            Box::new(Time(clock.clone())),
        )
        .unwrap();
        life.stop_on_silence(DEFAULT_SILENCE_STOP, contact.clone());
        Self {
            life,
            fake,
            clock,
            contact,
            connected: true,
            jobs: BTreeSet::new(),
            never_started: BTreeSet::new(),
            sent: Vec::new(),
            _dirs: (scratch, state),
        }
    }

    /// Notes the starts that were dropped as late.
    fn record(&mut self, events: &[Event]) {
        for event in events {
            if let Event::Skipped {
                run_id,
                why: Skip::Stale,
            } = event
            {
                self.never_started.insert(run_id.clone());
            }
        }
    }

    /// Delivers `reply` as one that arrived `waited` ms ago, then lets the worker step, as
    /// the agent's worker does after every order.
    fn deliver(&mut self, reply: &HeartbeatReply, waited: u64) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        let now = self.clock.get();
        self.contact.heard(now);
        let events = self.life.apply_at(reply, now.saturating_sub(waited));
        self.record(&events);
        let events = self.life.step();
        self.record(&events);
        Delivery::Delivered
    }

    /// The run ids and job ids of the results the lifecycle holds, with their details.
    fn results(&self) -> Vec<(String, Option<String>)> {
        let runs = self.life.results().into_iter();
        let runs = runs.map(|result| (result.run_id, result.detail));
        let jobs = self
            .life
            .held_jobs()
            .into_iter()
            .filter_map(|job| job.result);
        runs.chain(jobs.map(|result| (result.job_id, result.detail)))
            .collect()
    }
}

/// A run start for `run`, valid for `valid_for` ms (0 is unbounded).
fn run_start(run: &str, valid_for: u64) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: format!("lease-{run}"),
        image: IMAGE.into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            pids: None,
            wall_ms: 86_400_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: valid_for,
        bundle: None,
        native: None,
        network: Default::default(),
        ports: Vec::new(),
        run: None,
        os: None,
        arch: None,
    }
}

/// A job start of `job` under `allocation`.
fn job_start(job: &str, allocation: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job.into(),
        allocation_id: allocation.into(),
        image: IMAGE.into(),
        command: Vec::new(),
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 86_400_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
    }
}

fn of(commands: Vec<Command>) -> HeartbeatReply {
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

impl AgentHarness for JsAgent {
    fn fence_window(&self) -> Option<u64> {
        Some(u64::try_from(DEFAULT_SILENCE_STOP.as_millis()).unwrap())
    }

    fn now(&self) -> u64 {
        self.clock.get() - BASE
    }

    fn advance(&mut self, millis: u64) {
        self.clock.set(self.clock.get() + millis);
        let events = self.life.step();
        self.record(&events);
    }

    fn start(&mut self, lease: &str, valid_for: u64, waited: u64) -> Delivery {
        let start = Command::Start(Box::new(run_start(lease, valid_for)));
        self.deliver(&of(vec![start]), waited)
    }

    fn start_fenced(&mut self, job: &str, allocation: &str) -> Delivery {
        let start = Command::JobStart(Box::new(job_start(job, allocation)));
        let delivered = self.deliver(&of(vec![start]), 0);
        if delivered == Delivery::Delivered {
            self.jobs.insert(job.to_owned());
        }
        delivered
    }

    fn cancel(&mut self, lease: &str) -> Delivery {
        let command = match self.jobs.contains(lease) {
            true => Command::JobStop {
                job_id: lease.to_owned(),
            },
            false => Command::Cancel {
                run_id: lease.to_owned(),
            },
        };
        self.deliver(&of(vec![command]), 0)
    }

    fn ack(&mut self, lease: &str) -> Delivery {
        let reply = match self.jobs.contains(lease) {
            true => HeartbeatReply {
                job_acks: vec![JobResultAck {
                    job_id: lease.to_owned(),
                    ack: Ack::Accepted,
                }],
                ..HeartbeatReply::default()
            },
            false => HeartbeatReply {
                result_acks: vec![ResultAck {
                    run_id: lease.to_owned(),
                    ack: Ack::Accepted,
                }],
                ..HeartbeatReply::default()
            },
        };
        self.deliver(&reply, 0)
    }

    fn complete(&mut self, lease: &str) {
        self.fake.exit(lease);
        let events = self.life.step();
        self.record(&events);
        self.heartbeat();
    }

    fn heartbeat(&mut self) {
        if self.connected {
            let results = self.results();
            self.sent.extend(results.into_iter().map(|(id, _)| id));
        }
    }

    fn contact(&mut self) {
        self.contact.heard(self.clock.get());
    }

    fn disconnect(&mut self) {
        self.connected = false;
    }

    fn reconnect(&mut self) {
        self.connected = true;
        self.heartbeat();
    }

    fn observed(&self) -> Observed {
        let runs = self.life.held().into_iter();
        let runs = runs.filter(|run| run.state == HeldState::Running);
        let jobs = self.life.held_jobs().into_iter();
        let jobs = jobs.filter(|job| job.state == HeldState::Running);
        let running = runs
            .map(|run| run.run_id)
            .chain(jobs.map(|job| job.job_id))
            .collect();
        let silenced = Some(detail::SILENCE_STOP.to_owned());
        let fenced = self.results().into_iter();
        let fenced = fenced.filter(|(_, detail)| *detail == silenced);
        Observed {
            running,
            fenced: fenced.map(|(id, _)| id).collect(),
            never_started: self.never_started.clone(),
            launches: self.fake.launches(),
            results_sent: self.sent.clone(),
        }
    }
}

/// Asserts that the real agent's lifecycle passes every scenario of the suite, among them
/// `SelfFencesAfterSilence` with the agent's default silence bound as its window (the
/// lease runs one tick short of the bound counted from the last contact, and is stopped and
/// reported `silence-stop` at the bound) and `RetiredFenceIsRefused` (a job start whose
/// fence is retired launches nothing, the same job under a new allocation runs). It catches
/// an agent that does not stop its jobs after a silence, counts the silence from the wrong
/// moment, or keys its retired fences on anything but the job with its allocation.
#[test]
fn the_js_agent_passes_every_scenario() {
    let report = agent::run(JsAgent::new);
    assert_eq!(report.results.len(), Scenario::ALL.len());
    let failed: Vec<(Scenario, Verdict)> = report
        .results
        .iter()
        .filter(|(_, verdict)| *verdict != Verdict::Passed)
        .cloned()
        .collect();
    assert!(failed.is_empty(), "{failed:?}");
}

/// Asserts that the adapter reports the agent's own silence bound as its fence window, so
/// the fence scenario runs against the number the agent ships with. It catches an adapter
/// that passes the scenario with a window of its own.
#[test]
fn the_fence_window_is_the_agents_silence_bound() {
    let agent = JsAgent::new();
    assert_eq!(agent.fence_window(), Some(300_000));
    assert_eq!(agent.now(), 0);
}
