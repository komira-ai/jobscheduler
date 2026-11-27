//! The host-agent suite against the real js-agent lifecycle. The adapter drives
//! `Lifecycle` over a runtime that lives in memory and a clock the suite moves, and stamps
//! the agent's `Contact` where the session would: a message the harness delivers is a
//! reply the session accepted, and `contact` is an answered heartbeat. A `start` is the start
//! the agent makes of a run spec (`Lifecycle::start_spec`), a `start_fenced` a job start
//! whose fence is the job with its allocation; both are keyed by job. The fence window is the agent's default silence bound
//! (`--silence-stop`).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    Capacity, Clock, Config, Contact, DEFAULT_SILENCE_STOP, Exit, Found, Lifecycle, MountRoots,
    NoChecks, RunHandle, RunSpec, Runtime, RuntimeError, ScratchRoot, StateRoot, Status, detail,
};
use js_agent_wire::{
    Ack, BookedResources, Command, HeartbeatReply, HeldState, Isolation, JobResultAck, JobSpec,
    JobStartSpec, Limits,
};
use js_conformance::Verdict;
use js_conformance::agent::{self, AgentHarness, Delivery, Observed, Scenario};
use js_testkit::dir::TestDir;

const IMAGE: &str =
    "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Where the suite's clock starts on the lifecycle's: the suite reads the clock from 0.
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
            allow_fuse_jobs: false,
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
            sent: Vec::new(),
            _dirs: (scratch, state),
        }
    }

    /// Lets the agent act on a message the session accepted: it counts as contact, then
    /// `act` hands the lifecycle the message, then the worker steps, as the agent's worker
    /// does after every order.
    fn deliver(&mut self, act: impl FnOnce(&mut Lifecycle)) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.contact.heard(self.clock.get());
        act(&mut self.life);
        self.life.step();
        Delivery::Delivered
    }

    /// The job ids of the results the lifecycle holds, with their details.
    fn results(&self) -> Vec<(String, Option<String>)> {
        let jobs = self
            .life
            .held_jobs()
            .into_iter()
            .filter_map(|job| job.result);
        jobs.map(|result| (result.job_id, result.detail)).collect()
    }
}

/// A run spec for `run`, under the allocation `lease-<run>`.
fn run_spec(run: &str) -> JobSpec {
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
        isolation: Default::default(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: None,
        fuse: false,
        xcode: None,
        sizing: None,
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
        self.life.step();
    }

    fn start(&mut self, lease: &str) -> Delivery {
        let spec = run_spec(lease);
        self.deliver(|life| {
            life.start_spec(&spec);
        })
    }

    fn start_fenced(&mut self, job: &str, allocation: &str) -> Delivery {
        let start = Command::JobStart(Box::new(job_start(job, allocation)));
        let reply = of(vec![start]);
        self.deliver(|life| {
            life.apply(&reply);
        })
    }

    fn cancel(&mut self, lease: &str) -> Delivery {
        let command = Command::JobStop {
            job_id: lease.to_owned(),
        };
        let reply = of(vec![command]);
        self.deliver(|life| {
            life.apply(&reply);
        })
    }

    fn ack(&mut self, lease: &str) -> Delivery {
        let reply = HeartbeatReply {
            job_acks: vec![JobResultAck {
                job_id: lease.to_owned(),
                ack: Ack::Accepted,
            }],
            ..HeartbeatReply::default()
        };
        self.deliver(|life| {
            life.apply(&reply);
        })
    }

    fn complete(&mut self, lease: &str) {
        self.fake.exit(lease);
        self.life.step();
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
        let jobs = self.life.held_jobs().into_iter();
        let jobs = jobs.filter(|job| job.state == HeldState::Running);
        let running = jobs.map(|job| job.job_id).collect();
        let silenced = Some(detail::SILENCE_STOP.to_owned());
        let fenced = self.results().into_iter();
        let fenced = fenced.filter(|(_, detail)| *detail == silenced);
        Observed {
            running,
            fenced: fenced.map(|(id, _)| id).collect(),
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
