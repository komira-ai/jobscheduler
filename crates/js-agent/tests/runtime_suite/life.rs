//! The cases that drive a lifecycle over the runtime: what the lifecycle asks of any
//! runtime, and when.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    Clock, Event, Found, Lifecycle, NoChecks, Prepared, RunHandle, RunSpec, Runtime, RuntimeError,
    Status, Ticket,
};
use js_agent_wire::{Command, HeartbeatReply, Isolation, JobSpec, Limits, Mount, Outcome};

use super::{LONG, Program, Shared, Subject, clean_up};

/// A clock that moves when a case says so. Clones share one reading.
#[derive(Clone, Default)]
pub struct ManualClock(Rc<Cell<u64>>);

impl ManualClock {
    /// Moves the clock on by `ms`.
    pub fn advance(&self, ms: u64) {
        self.0.set(self.0.get() + ms);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// The runtime as the lifecycle sees it: every call is passed on and the ones a case reads
/// are noted in the log (`egress`, `create <run>`, `stop <ms>ms`, `exit <code>`).
struct Probe {
    inner: Shared,
    log: Rc<RefCell<Vec<String>>>,
    egress_fails: Rc<Cell<bool>>,
}

impl Probe {
    fn note(&self, line: String) {
        self.log.borrow_mut().push(line);
    }
}

impl Runtime for Probe {
    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.inner.borrow_mut().prepare(image, waited)
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        self.note(format!("create {}", spec.run_id));
        self.inner.borrow_mut().create_and_start(spec)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        let status = self.inner.borrow_mut().poll(id);
        if let Ok(Status::Exited(exit)) = &status {
            self.note(format!("exit {}", exit.code));
        }
        status
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.note(format!("stop {}ms", grace.as_millis()));
        self.inner.borrow_mut().stop(id, grace)
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.inner.borrow_mut().remove(id)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.inner.borrow_mut().list_labelled()
    }

    fn supports_gpus(&self) -> bool {
        self.inner.borrow().supports_gpus()
    }

    fn check_egress(&self) -> Result<(), RuntimeError> {
        self.note("egress".to_owned());
        match self.egress_fails.get() {
            true => Err(RuntimeError::new("the firewall changed")),
            false => self.inner.borrow().check_egress(),
        }
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        self.inner.borrow_mut().cgroup(id)
    }
}

/// A lifecycle over the subject's runtime, with the pieces a case reads.
pub struct Rig {
    life: Lifecycle,
    clock: ManualClock,
    log: Rc<RefCell<Vec<String>>>,
    egress_fails: Rc<Cell<bool>>,
}

impl Rig {
    /// A lifecycle whose stops give a container `grace`.
    pub fn new(s: &mut dyn Subject, grace: Duration) -> Self {
        let mut config = s.config();
        config.stop_grace = grace;
        let log = Rc::new(RefCell::new(Vec::new()));
        let egress_fails = Rc::new(Cell::new(false));
        let clock = ManualClock::default();
        let probe = Probe {
            inner: s.runtime(),
            log: Rc::clone(&log),
            egress_fails: Rc::clone(&egress_fails),
        };
        let (life, _) = Lifecycle::adopt(
            config,
            Box::new(probe),
            Box::new(NoChecks),
            Box::new(clock.clone()),
        )
        .expect("a lifecycle over an empty runtime");
        Self {
            life,
            clock,
            log,
            egress_fails,
        }
    }

    /// Starts `spec`; the events as short texts.
    fn start(&mut self, spec: JobSpec) -> Vec<String> {
        let reply = HeartbeatReply {
            commands: vec![Command::Start(Box::new(spec))],
            ..HeartbeatReply::default()
        };
        names(&self.life.apply(&reply))
    }

    /// The log, and empties it.
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.borrow_mut())
    }
}

/// The events as short texts.
fn names(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Event::Started { run_id } => format!("started {run_id}"),
            Event::Refused { run_id, code, .. } => format!("refused {run_id} {code}"),
            Event::ResultReady { run_id } => format!("result {run_id}"),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A start of the run `run_id` of `program`, which may run for `wall_ms`.
fn job(s: &mut dyn Subject, run_id: &str, program: Program, wall_ms: u64) -> JobSpec {
    s.sync(run_id);
    let ready = format!("/mnt/sync/{run_id}/ready");
    JobSpec {
        run_id: run_id.to_owned(),
        lease_id: format!("lease-{run_id}"),
        image: s.image(),
        args: program.args(&ready),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: program.memory_bytes(),
            disk_bytes: None,
            pids: Some(256),
            wall_ms,
        },
        gpu_indices: Vec::new(),
        mounts: vec![Mount {
            root_name: "sync".to_owned(),
            rel: run_id.to_owned(),
            ro: false,
        }],
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
    }
}

/// Runs a job of `program` with a wall limit of ten seconds on the lifecycle's clock past
/// it, with stops that give `grace`, and returns the log of the runtime's calls after the
/// start and the outcome of the run.
fn past_its_wall_time(
    s: &mut dyn Subject,
    program: Program,
    grace: Duration,
) -> (Vec<String>, Outcome) {
    s.prepared();
    s.before(program);
    let mut rig = Rig::new(s, grace);
    let spec = job(s, "r1", program, 10_000);
    assert_eq!(rig.start(spec), ["started r1"]);
    s.after("r1", program);
    rig.taken();
    rig.clock.advance(9_999);
    assert!(rig.life.step().is_empty());
    assert!(rig.taken().is_empty());
    rig.clock.advance(1);
    assert_eq!(names(&rig.life.step())[0], "result r1");
    let outcome = rig.life.results()[0].outcome;
    let log = rig.taken();
    clean_up(s);
    (log, outcome)
}

/// Proves the wall-time limit is read from the lifecycle's clock and ends the job with a
/// stop that gives it the configured grace: nothing is done one millisecond before the
/// limit; at it the runtime is stopped once, the job that handles SIGTERM exits with its
/// own code (7), not the kill's 137, and the run is reported `timed_out`. Catches a limit
/// that is never enforced or early, and a stop that kills before the job can end.
pub fn the_wall_time_ends_a_job_with_sigterm(s: &mut dyn Subject) {
    let (log, outcome) = past_its_wall_time(s, Program::OnTerm(7), LONG);
    assert_eq!(log, ["stop 3600000ms", "exit 7"]);
    assert_eq!(outcome, Outcome::TimedOut);
}

/// Proves the kill when the grace is spent: with a grace of zero, a job that ignores
/// SIGTERM is killed at the wall time and exits 137. Catches a stop that leaves the job
/// running past its limit.
pub fn the_wall_time_kills_a_job_that_ignores_sigterm(s: &mut dyn Subject) {
    let (log, outcome) = past_its_wall_time(s, Program::IgnoreTerm, Duration::ZERO);
    assert_eq!(log, ["stop 0ms", "exit 137"]);
    assert_eq!(outcome, Outcome::TimedOut);
}

/// Proves the lifecycle asks the runtime whether its network is as told before every
/// contained create, not once: two starts give `egress, create` twice in that order, and a
/// start whose check fails is refused `egress-unchecked` and creates nothing. Catches a
/// check made once at start-up, after the create, or ignored.
pub fn egress_is_checked_before_every_create(s: &mut dyn Subject) {
    s.prepared();
    let mut rig = Rig::new(s, LONG);
    for run in ["r1", "r2"] {
        let spec = job(s, run, Program::Hold, 600_000);
        assert_eq!(rig.start(spec), [format!("started {run}")]);
        s.after(run, Program::Hold);
    }
    assert_eq!(rig.taken(), ["egress", "create r1", "egress", "create r2"]);
    rig.egress_fails.set(true);
    let spec = job(s, "r3", Program::Hold, 600_000);
    assert_eq!(rig.start(spec), ["refused r3 egress-unchecked"]);
    assert_eq!(rig.taken(), ["egress"]);
    clean_up(s);
}

/// Proves a start that names a GPU on a runtime without GPUs is refused `gpu-unsupported`
/// and the runtime is never asked to create it. Catches a job that runs without the
/// device it was booked.
pub fn a_gpu_request_is_refused_and_nothing_is_created(s: &mut dyn Subject) {
    s.prepared();
    let mut rig = Rig::new(s, LONG);
    let mut spec = job(s, "g1", Program::Hold, 600_000);
    spec.gpu_indices = vec![0];
    assert_eq!(rig.start(spec), ["refused g1 gpu-unsupported"]);
    assert!(!rig.taken().iter().any(|line| line.starts_with("create")));
}
