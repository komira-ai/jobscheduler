//! The host-agent suite against the real js-agent lifecycle. The adapter drives
//! `Lifecycle` over the in-memory runtime (`js_conformance::agent::memory`) and a clock the
//! suite moves, and stamps the agent's `Contact` where the session would: a message the
//! harness delivers is a reply the session accepted, and `contact` is an answered heartbeat.
//! A `start` is the start the agent makes of a run spec (`Lifecycle::start_spec`), a
//! `start_fenced` a job start whose fence is the job with its allocation; both are keyed by
//! job. The fence window is the agent's default silence bound (`--silence-stop`).
//!
//! The runtime fails the removal of the runs the suite names (`remove_fails`) and reads a
//! usage for every job that exited; the post-job check finds what the suite names
//! (`check_finds`) and nothing else. A restart builds a new lifecycle over the same runtime,
//! state and scratch directories (`Lifecycle::adopt`, as the agent's process does when it
//! starts), which takes over the containers the agent made (each is labelled as keyed by
//! job); the operator's consent to host jobs is a restart with `allow_host_jobs`. The
//! whole-node lease is the `exclusive` of every reply after `hold_whole`, and its readiness
//! is the lifecycle's answer to `prepare`, which the session asks on each heartbeat.
//!
//! A flawed start goes where the agent refuses it. Four rules are the wire's check of a job
//! start (no wall time, a fence with no allocation, a VM, which the agent has no backend
//! for) or the agent's admission of one (a GPU on a shape that has none), and are sent as
//! `JobStart`. The rules only a `JobSpec` carries are sent through `Lifecycle::start_spec`:
//! a mount outside the agent's roots is refused by the agent's admission, and a label under
//! the reserved prefix by the wire's check of the spec (`JobSpec::validate`), which
//! `start_spec` does not run, so the adapter runs it first and starts nothing the check
//! refuses.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::time::Duration;

use js_agent::integrity::code::{PROCESS_AS_JOB_USER, PROCESSES_UNREADABLE};
use js_agent::lifecycle::{
    Capacity, CheckFinding, CheckHook, Config, Contact, DEFAULT_SILENCE_STOP, Event, Finished,
    Found, Lifecycle, MountRoots, Quiet, RunHandle, RunSpec, Runtime, RuntimeError, ScratchRoot,
    StateRoot, Status, detail,
};
use js_agent_wire::{
    Ack, BookedResources, Command, HeartbeatReply, HeldState, Isolation, JobResultAck, JobSpec,
    JobStartSpec, JobUsage, Limits, Mount, StartKind,
};
use js_conformance::Verdict;
use js_conformance::agent::memory::{Memory, Time};
use js_conformance::agent::{
    self, AgentHarness, Answer, Cleanup, Delivery, Flaw, HostCheck, Observed, Scenario,
};
use js_testkit::dir::TestDir;

const IMAGE: &str =
    "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Where the suite's clock starts on the lifecycle's: the suite reads the clock from 0.
const BASE: u64 = 1 << 40;

/// How many steps the adapter lets the lifecycle take after each message or exit, as the
/// agent's worker steps after every order and on every tick: enough for a stop to reach the
/// runtime, the container to exit and its cleanup to finish.
const SETTLE_STEPS: usize = 3;

/// The exit status of a job that fails.
const FAILED: i32 = 1;

/// What every exited job used, as the runtime reads it.
const USAGE: JobUsage = JobUsage {
    peak_memory_bytes: 1 << 20,
    cpu_usage_usec: 1_000,
    cpu_throttle: None,
    cpu_pressure_some_usec: None,
    cpu_peak: None,
    peak_swap_bytes: None,
};

/// The in-memory runtime, whose removal of a container fails for the runs the suite named,
/// and which reads [`USAGE`] for every container. Clones share everything.
#[derive(Clone, Default)]
struct Fake {
    memory: Memory,
    /// The run of each container, by the container's id.
    runs: Rc<RefCell<BTreeMap<String, String>>>,
    /// The runs whose container cannot be removed.
    stuck: Rc<RefCell<BTreeSet<String>>>,
}

impl Fake {
    /// How many containers were made for each run.
    fn launches(&self) -> BTreeMap<String, u32> {
        let mut launches = BTreeMap::new();
        for run in self.memory.created() {
            *launches.entry(run).or_insert(0) += 1;
        }
        launches
    }
}

impl Runtime for Fake {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let handle = self.memory.create_and_start(spec)?;
        let mut runs = self.runs.borrow_mut();
        runs.insert(handle.0.clone(), spec.run_id.clone());
        Ok(handle)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        self.memory.poll(id)
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.memory.stop(id, grace)
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let run = self.runs.borrow().get(&id.0).cloned().unwrap_or_default();
        if self.stuck.borrow().contains(&run) {
            return Err(RuntimeError::new("the container could not be removed"));
        }
        self.memory.remove(id)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.memory.list_labelled()
    }

    fn usage(&mut self, _id: &RunHandle) -> Option<JobUsage> {
        Some(USAGE)
    }
}

/// The post-job check: it finds what the suite named for a run, and nothing after any other.
#[derive(Clone, Default)]
struct Hook(Rc<RefCell<BTreeMap<String, HostCheck>>>);

impl CheckHook for Hook {
    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        match self.0.borrow().get(job.run_id) {
            None => Ok(()),
            Some(HostCheck::LeftoverProcess) => Err(CheckFinding::residue(
                PROCESS_AS_JOB_USER,
                "a process of the job's user is left",
            )),
            Some(HostCheck::CannotRun) => Err(CheckFinding::unchecked(
                PROCESSES_UNREADABLE,
                "the host's processes could not be read",
            )),
        }
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
    config: Config,
    fake: Fake,
    hook: Hook,
    clock: Rc<Cell<u64>>,
    contact: Contact,
    connected: bool,
    /// The lease the host is held whole for, which every reply names.
    whole: Option<String>,
    /// How many commands the lifecycle refused as malformed.
    invalid: usize,
    refused: BTreeSet<String>,
    findings: BTreeSet<String>,
    cleanup: Vec<(String, Cleanup)>,
    sent: Vec<String>,
    taint_reports: u32,
    /// The scratch and state roots, removed after the lifecycle that uses them is gone.
    _dirs: (TestDir, TestDir),
}

/// A lifecycle over `fake` that takes over what the runtime and the state root hold, as
/// the agent's process does when it starts, with the agent's default silence bound.
fn boot(
    config: &Config,
    (fake, hook): (&Fake, &Hook),
    clock: &Rc<Cell<u64>>,
    contact: &Contact,
) -> (Lifecycle, Vec<Event>) {
    let (mut life, events) = Lifecycle::adopt(
        config.clone(),
        Box::new(fake.clone()),
        Box::new(hook.clone()),
        Box::new(Time(clock.clone())),
    )
    .unwrap();
    life.stop_on_silence(DEFAULT_SILENCE_STOP, contact.clone());
    (life, events)
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
        let (fake, hook) = (Fake::default(), Hook::default());
        let (clock, contact) = (Rc::new(Cell::new(BASE)), Contact::default());
        let (life, _) = boot(&config, (&fake, &hook), &clock, &contact);
        Self {
            life,
            config,
            fake,
            hook,
            clock,
            contact,
            connected: true,
            whole: None,
            invalid: 0,
            refused: BTreeSet::new(),
            findings: BTreeSet::new(),
            cleanup: Vec::new(),
            sent: Vec::new(),
            taint_reports: 0,
            _dirs: (scratch, state),
        }
    }

    /// Notes what the lifecycle reported: the starts refused, the commands refused as
    /// malformed, the steps of each cleanup and the findings.
    fn record(&mut self, events: &[Event]) {
        for event in events {
            match event {
                Event::Refused { run_id, .. } => {
                    self.refused.insert(run_id.clone());
                }
                Event::Invalid { .. } => self.invalid += 1,
                Event::ContainerRemoved { run_id } => {
                    self.cleanup.push((run_id.clone(), Cleanup::ContainerGone));
                }
                Event::ScratchRemoved { run_id } => {
                    self.cleanup.push((run_id.clone(), Cleanup::ScratchGone));
                }
                Event::SlotFreed { run_id } => {
                    self.cleanup.push((run_id.clone(), Cleanup::SlotFreed));
                }
                Event::Finding(finding) => {
                    self.findings.extend(finding.run_id.clone());
                }
                _ => {}
            }
        }
    }

    /// Lets the lifecycle take [`SETTLE_STEPS`] steps.
    fn settle(&mut self) {
        for _ in 0..SETTLE_STEPS {
            let events = self.life.step();
            self.record(&events);
        }
    }

    /// Lets the agent act on a message the session accepted: it counts as contact, then
    /// `act` hands the lifecycle the message, then the lifecycle settles, as the agent's
    /// worker steps after every order.
    fn deliver(&mut self, act: impl FnOnce(&mut Lifecycle) -> Vec<Event>) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.contact.heard(self.clock.get());
        let events = act(&mut self.life);
        self.record(&events);
        self.settle();
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

    /// The running container of `run` exits with `code`, and the agent reports it.
    fn exit(&mut self, run: &str, code: i32) {
        self.fake.memory.exit(run, code);
        self.settle();
        self.heartbeat();
    }

    /// Delivers `reply`, naming the lease the host is held whole for, as one the session
    /// accepted.
    fn reply(&mut self, reply: HeartbeatReply) -> Delivery {
        let reply = HeartbeatReply {
            exclusive: self.whole.clone(),
            ..reply
        };
        self.deliver(|life| life.apply(&reply))
    }

    /// Delivers the job start `job` alone, and notes `lease` refused when the lifecycle
    /// refused the command as malformed.
    fn deliver_job(&mut self, lease: &str, job: JobStartSpec) -> Delivery {
        let before = self.invalid;
        let delivered = self.reply(of(vec![Command::JobStart(Box::new(job))]));
        if self.invalid > before {
            self.refused.insert(lease.to_owned());
        }
        delivered
    }

    /// Starts `spec` as [`Lifecycle::start_spec`] does, after the wire's check of it
    /// (`JobSpec::validate`), which `start_spec` does not run: a spec the check refuses is
    /// started nowhere, and `lease` is noted refused.
    fn deliver_spec(&mut self, lease: &str, spec: &JobSpec) -> Delivery {
        let refused = spec.validate().is_err();
        let delivered = self.deliver(|life| match refused {
            true => Vec::new(),
            false => life.start_spec(spec),
        });
        if refused && delivered == Delivery::Delivered {
            self.refused.insert(lease.to_owned());
        }
        delivered
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
        let events = self.life.step();
        self.record(&events);
    }

    fn start(&mut self, lease: &str) -> Delivery {
        let spec = run_spec(lease);
        self.deliver(|life| life.start_spec(&spec))
    }

    fn start_fenced(&mut self, job: &str, allocation: &str) -> Delivery {
        let start = Command::JobStart(Box::new(job_start(job, allocation)));
        self.reply(of(vec![start]))
    }

    fn cancel(&mut self, lease: &str) -> Delivery {
        let command = Command::JobStop {
            job_id: lease.to_owned(),
        };
        self.reply(of(vec![command]))
    }

    fn answer(&mut self, lease: &str, answer: Answer) -> Delivery {
        let ack = match answer {
            Answer::Accepted => Ack::Accepted,
            Answer::Duplicate => Ack::Duplicate,
            Answer::Stale => Ack::Stale,
            Answer::Mismatch => Ack::Mismatch,
            Answer::Unrecognised => Ack::Unknown,
        };
        let reply = HeartbeatReply {
            job_acks: vec![JobResultAck {
                job_id: lease.to_owned(),
                ack,
            }],
            ..HeartbeatReply::default()
        };
        self.reply(reply)
    }

    fn complete(&mut self, lease: &str) {
        self.exit(lease, 0);
    }

    fn fail(&mut self, lease: &str) {
        self.exit(lease, FAILED);
    }

    fn start_flawed(&mut self, lease: &str, flaw: Flaw) -> Delivery {
        match flaw {
            Flaw::NoWallTime => {
                let mut job = job_start(lease, "first");
                job.wall_ms = 0;
                self.deliver_job(lease, job)
            }
            Flaw::UnstatedFence => {
                let mut job = job_start(lease, "first");
                job.allocation_id = String::new();
                self.deliver_job(lease, job)
            }
            Flaw::VmWithoutBackend => {
                let mut job = job_start(lease, "first");
                job.kind = StartKind::Vm;
                self.deliver_job(lease, job)
            }
            Flaw::GpuOnGpulessShape => {
                let mut job = job_start(lease, "first");
                job.booked.gpu_indices.push(0);
                self.deliver_job(lease, job)
            }
            Flaw::MountOutsideRoots => {
                let mut spec = run_spec(lease);
                spec.mounts.push(Mount {
                    root_name: "data".to_owned(),
                    rel: "input".to_owned(),
                    ro: true,
                });
                self.deliver(|life| life.start_spec(&spec))
            }
            Flaw::ReservedLabel => {
                let mut spec = run_spec(lease);
                let key = format!("{}flaw", js_types::RESERVED_PREFIX);
                spec.labels.insert(key, "set".to_owned());
                self.deliver_spec(lease, &spec)
            }
        }
    }

    fn remove_fails(&mut self, lease: &str) {
        self.fake.stuck.borrow_mut().insert(lease.to_owned());
    }

    fn check_finds(&mut self, lease: &str, check: HostCheck) {
        self.hook.0.borrow_mut().insert(lease.to_owned(), check);
    }

    fn consent_to_host_jobs(&mut self) {
        self.config.allow_host_jobs = true;
        self.restart();
    }

    fn start_host(&mut self, lease: &str) -> Delivery {
        let mut spec = run_spec(lease);
        spec.isolation = Isolation::Host;
        self.deliver(|life| life.start_spec(&spec))
    }

    fn ack_taint(&mut self) -> Delivery {
        let reply = HeartbeatReply {
            taint_ack: true,
            ..HeartbeatReply::default()
        };
        self.reply(reply)
    }

    fn restart(&mut self) {
        let (life, events) = boot(
            &self.config,
            (&self.fake, &self.hook),
            &self.clock,
            &self.contact,
        );
        self.life = life;
        self.record(&events);
        self.connected = true;
        self.settle();
        self.heartbeat();
        self.reply(HeartbeatReply::default());
    }

    fn hold_whole(&mut self, lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.whole = Some(lease.to_owned());
        self.reply(HeartbeatReply::default())
    }

    fn whole_ready(&mut self) -> bool {
        let Some(lease) = self.whole.clone() else {
            return false;
        };
        let mut events = Vec::new();
        let quiet = self.life.prepare(&lease, &mut events);
        self.record(&events);
        matches!(quiet, Quiet::Ready)
    }

    fn start_whole(&mut self, lease: &str) -> Delivery {
        let mut spec = run_spec(lease);
        spec.lease_id = self.whole.clone().unwrap_or_default();
        self.deliver(|life| life.start_spec(&spec))
    }

    fn heartbeat(&mut self) {
        if self.connected {
            let results = self.results();
            self.sent.extend(results.into_iter().map(|(id, _)| id));
            if self.life.pending_taint().is_some() {
                self.taint_reports += 1;
            }
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
        let held = self.life.held_jobs().into_iter();
        let used = held.filter(|job| job.result.as_ref().is_some_and(|r| r.usage.is_some()));
        Observed {
            running,
            fenced: fenced.map(|(id, _)| id).collect(),
            launches: self.fake.launches(),
            results_sent: self.sent.clone(),
            refused: self.refused.clone(),
            usage: used.map(|job| job.job_id).collect(),
            cleanup: self.cleanup.clone(),
            tainted: self.life.is_tainted(),
            taint_reports: self.taint_reports,
            findings: self.findings.clone(),
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
