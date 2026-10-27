//! The state machine: one [`Job`] per run the agent holds, moved by the commands of a
//! reply ([`Lifecycle::apply`]) and by the passing of time ([`Lifecycle::step`]).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use js_agent_wire::{
    Ack, Command, HeartbeatReply, HeldRun, HeldState, Isolation, JobSpec, Outcome, ResultAck,
    RunResult, limits, refusal,
};

use super::runtime::{
    ContainerId, ContainerRuntime, ContainerSpec, Exit, Found, ResolvedMount, Status,
};
use super::{
    Capacity, CheckFinding, CheckHook, Clock, Config, Event, Finding, RuntimeError, Skip, code,
    detail, label,
};
/// How many finished or stopped run ids the agent remembers, so a late or repeated start
/// for one of them runs nothing. The oldest is forgotten first; a start for a run older
/// than the last [`RETIRED_CAP`] is the server's to stop sending.
pub const RETIRED_CAP: usize = limits::MAX_HELD;

/// The most jobs the agent holds, each listed in the held runs of a heartbeat. A job
/// carries at most one result, so the results stay within the wire limit as well.
const MAX_JOBS: usize = limits::MAX_HELD;
const _: () = assert!(limits::MAX_HELD <= limits::MAX_RESULTS);

/// Where a job is. The container's id travels with the stages that need it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// The container runs.
    Running(ContainerId),
    /// The container has exited; it is to be removed.
    Removing(ContainerId),
    /// The scratch directory is to be removed.
    Scratch,
    /// The post-job checks are to run.
    Checks,
    /// A check found something: the slot stays booked.
    Blocked,
    /// The slot is free; the job is kept only for its unacknowledged result.
    Freed,
}

/// Why the agent is ending a job. It decides the result only when the agent's stop is what
/// ended the container (or the container is gone); a container that exited before a stop
/// reached it is reported as it exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// A `Stop` or `Cancel`.
    Stopped,
    /// The wall-time limit.
    TimedOut,
}

/// What a job holds of the host while its slot is booked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Booking {
    cpu_millis: u64,
    memory_bytes: u64,
    gpus: Vec<u32>,
}

impl Booking {
    /// Whether this booking fits beside `used` in what `offered` has: the CPU and memory
    /// within what is left, and every GPU offered and not used.
    fn fits(&self, offered: &Capacity, used: &Capacity) -> bool {
        [
            self.cpu_millis <= offered.cpu_millis.saturating_sub(used.cpu_millis),
            self.memory_bytes <= offered.memory_bytes.saturating_sub(used.memory_bytes),
            self.gpus
                .iter()
                .all(|gpu| offered.gpus.contains(gpu) && !used.gpus.contains(gpu)),
        ]
        .into_iter()
        .all(|ok| ok)
    }

    /// What the labels of a container say it booked: all three are numbers and no GPU is
    /// named twice (the agent never writes such a booking).
    fn from_labels(found: &Found) -> Option<Self> {
        let gpus = found
            .labels
            .get(label::GPUS)?
            .split(',')
            .filter(|index| !index.is_empty())
            .map(|index| index.parse::<u32>().ok())
            .collect::<Option<Vec<_>>>()?;
        let distinct: BTreeSet<&u32> = gpus.iter().collect();
        (distinct.len() == gpus.len()).then_some(())?;
        Some(Self {
            cpu_millis: label_number(found, label::CPU_MILLIS)?,
            memory_bytes: label_number(found, label::MEMORY_BYTES)?,
            gpus,
        })
    }

    /// All that `offered` has: what is held for a container whose booking is not known.
    fn everything(offered: &Capacity) -> Self {
        Self {
            cpu_millis: offered.cpu_millis,
            memory_bytes: offered.memory_bytes,
            gpus: offered.gpus.iter().copied().collect(),
        }
    }
}

/// The label `key` of a found container, as a number.
fn label_number(found: &Found, key: &str) -> Option<u64> {
    found.labels.get(key)?.parse().ok()
}

/// A container the agent could not remove: it may still run, so what it booked, or the
/// whole offer when that is not known, stays booked until a removal succeeds.
struct Stuck {
    id: ContainerId,
    booking: Booking,
}

struct Job {
    run_id: String,
    lease_id: String,
    booking: Booking,
    /// On the agent's clock.
    deadline_ms: u64,
    stage: Stage,
    cause: Option<Cause>,
    /// A cancelled job reports nothing.
    silent: bool,
    stop_sent: bool,
    result: Option<RunResult>,
    findings: Vec<Finding>,
}

impl Job {
    fn report(&self, outcome: Outcome, exit_code: Option<i32>, detail: Option<&str>) -> RunResult {
        RunResult {
            run_id: self.run_id.clone(),
            lease_id: self.lease_id.clone(),
            outcome,
            exit_code,
            detail: detail.map(str::to_owned),
        }
    }

    /// Records a finding unless the job already has one of this code.
    fn note(&mut self, code: &'static str, message: String, events: &mut Vec<Event>) {
        if self.findings.iter().all(|found| found.code != code) {
            let finding = Finding {
                run_id: Some(self.run_id.clone()),
                code,
                message,
            };
            self.findings.push(finding.clone());
            events.push(Event::Finding(finding));
        }
    }

    /// Asks a running job to stop; a job past its exit has nothing to stop.
    fn request_stop(&mut self, silent: bool) {
        if matches!(self.stage, Stage::Running(_)) {
            self.cause.get_or_insert(Cause::Stopped);
            self.silent |= silent;
        }
    }
}

/// The outcome of a container that exited (`Some`) or vanished (`None`), and the exit
/// status when the job's own exit is what ended it.
fn outcome(cause: Option<Cause>, exit: Option<Exit>) -> (Outcome, Option<i32>) {
    match (cause, exit) {
        (Some(Cause::TimedOut), _) => (Outcome::TimedOut, None),
        (Some(Cause::Stopped), _) => (Outcome::Killed, None),
        (
            None,
            Some(Exit {
                code,
                oom_killed: true,
            }),
        ) => (Outcome::OutOfMemory, Some(code)),
        (None, Some(Exit { code: 0, .. })) => (Outcome::Succeeded, Some(0)),
        (None, Some(Exit { code, .. })) => (Outcome::Failed, Some(code)),
        (None, None) => (Outcome::Failed, None),
    }
}

/// The run ids that must never start again, oldest first.
#[derive(Default)]
struct Retired {
    order: VecDeque<String>,
    seen: BTreeSet<String>,
}

impl Retired {
    fn insert(&mut self, run_id: &str) {
        if self.seen.insert(run_id.to_owned()) {
            self.order.push_back(run_id.to_owned());
        }
        if self.order.len() > RETIRED_CAP {
            self.order.pop_front().iter().for_each(|old| {
                self.seen.remove(old);
            });
        }
    }

    fn contains(&self, run_id: &str) -> bool {
        self.seen.contains(run_id)
    }
}

/// A refusal: the wire code and a local explanation.
type Refusal = (&'static str, String);

/// The jobs of one agent. See the [module](super) for what it guarantees.
pub struct Lifecycle {
    config: Config,
    runtime: Box<dyn ContainerRuntime>,
    checks: Box<dyn CheckHook>,
    clock: Box<dyn Clock>,
    jobs: Vec<Job>,
    /// Containers no job stands for that could not be removed; not held runs.
    stuck: Vec<Stuck>,
    retired: Retired,
}

impl Lifecycle {
    /// Starts the lifecycle of an agent that has just started, taking over the labelled
    /// containers it finds so that they are held, first, from the very first heartbeat. A
    /// container that no job can stand for is stopped and removed by its own id, and
    /// recorded as a finding: one whose labels do not say what it booked, one that repeats
    /// a run id, one whose booking does not fit the machine's offer beside the containers
    /// taken over before it (a GPU not offered, or one already booked), and any beyond the
    /// number of runs a heartbeat can list. One that cannot be removed is not left
    /// unbooked: it holds what its labels say it booked (the whole offer when they do not
    /// say), and the stop and removal are tried again at every [`Lifecycle::step`] until
    /// the removal succeeds. It is not a held run, and it never costs a running job its
    /// place.
    ///
    /// The containers are taken over in the order of their age: those that have exited
    /// (their results are held) first, then the rest, the longest running first, whatever
    /// order the runtime lists them in. When the offer is lower than at the last start,
    /// the jobs that no longer fit are therefore the newest, and they are evicted.
    ///
    /// What the runtime lists is the agent's own ([`ContainerRuntime::list_labelled`] is
    /// scoped by the agent label), including the removal of those that cannot be adopted.
    ///
    /// # Errors
    ///
    /// The runtime cannot list its containers. The caller must not say hello with an
    /// empty held list then: it does not know what it holds.
    pub fn adopt(
        config: Config,
        mut runtime: Box<dyn ContainerRuntime>,
        checks: Box<dyn CheckHook>,
        clock: Box<dyn Clock>,
    ) -> Result<(Self, Vec<Event>), RuntimeError> {
        let mut found: Vec<(bool, Found)> = runtime
            .list_labelled()?
            .into_iter()
            .map(|container| {
                let exited = matches!(runtime.poll(&container.id), Ok(Status::Exited(_)));
                (exited, container)
            })
            .collect();
        found.sort_by_key(|(exited, container)| (!exited, Reverse(container.running_ms)));
        let mut this = Self {
            config,
            runtime,
            checks,
            clock,
            jobs: Vec::new(),
            stuck: Vec::new(),
            retired: Retired::default(),
        };
        let mut events = Vec::new();
        let mut stuck = Vec::new();
        for (_, container) in found {
            let now = this.clock.now_ms();
            match this.adoptable(&container, now) {
                Ok(job) => {
                    events.push(Event::Adopted {
                        run_id: job.run_id.clone(),
                    });
                    this.jobs.push(job);
                }
                Err(why) => stuck.extend(this.evict(&container, why, &mut events)),
            }
        }
        // Booked only now, so that a container that cannot be removed never makes a
        // container that runs look like one that does not fit.
        this.stuck = stuck;
        Ok((this, events))
    }

    /// The job a labelled container stands for, or why it cannot be one: its labels do not
    /// say what it booked, its run id is held already, the agent holds as many runs as it
    /// may, or its booking does not fit what the machine offers beside the jobs taken over
    /// before it.
    fn adoptable(&self, found: &Found, now: u64) -> Result<Job, &'static str> {
        let job = Job::adopted(found, now).ok_or("its labels do not say what it booked")?;
        let used = self.booked();
        let refusals = [
            (
                self.jobs.iter().any(|held| held.run_id == job.run_id),
                "its run id is held already",
            ),
            (
                self.jobs.len() >= MAX_JOBS,
                "the agent holds as many runs as a heartbeat can list",
            ),
            (
                !job.booking.fits(&self.config.capacity, &used),
                "its booking does not fit the machine beside the jobs taken over",
            ),
        ];
        match refusals.into_iter().find(|(refused, _)| *refused) {
            Some((_, why)) => Err(why),
            None => Ok(job),
        }
    }

    /// Stops and removes a container no job stands for, so that nothing runs on the host
    /// unbooked, and records it. A stop that fails does not keep the removal from being
    /// tried (removal takes a running container too); a removal that fails is the louder
    /// finding, because the container is then still there, and it is returned to be booked
    /// and tried again.
    fn evict(&mut self, found: &Found, why: &str, events: &mut Vec<Event>) -> Option<Stuck> {
        let id = &found.id;
        let _ = self.runtime.stop(id, self.config.stop_grace);
        let (code, fate, stuck) = match self.runtime.remove(id) {
            Ok(()) => (code::UNADOPTABLE, "stopped and removed".to_owned(), None),
            Err(failed) => (
                code::UNADOPTABLE_KEPT,
                format!("not removed, still on the host and booked: {failed}"),
                Some(Stuck {
                    id: id.clone(),
                    booking: Booking::from_labels(found)
                        .unwrap_or_else(|| Booking::everything(&self.config.capacity)),
                }),
            ),
        };
        events.push(Event::Finding(Finding {
            run_id: None,
            code,
            message: format!("container {}: {why}; {fate}", id.0),
        }));
        stuck
    }

    /// Tries again to stop and remove the containers that could not be removed; the
    /// booking of one that is gone is freed.
    fn retry_stuck(&mut self, events: &mut Vec<Event>) {
        let Self {
            stuck,
            runtime,
            config,
            ..
        } = self;
        stuck.retain(|held| {
            let _ = runtime.stop(&held.id, config.stop_grace);
            let gone = runtime.remove(&held.id).is_ok();
            if gone {
                events.push(Event::Finding(Finding {
                    run_id: None,
                    code: code::UNADOPTABLE,
                    message: format!("container {}: removed on a later attempt", held.id.0),
                }));
            }
            !gone
        });
    }

    /// Acts on a heartbeat reply: validates it as the wire demands, starts what can be
    /// started, stops what is to be stopped and drops the results the server
    /// acknowledged. A command that fails validation is skipped and reported; the commands
    /// beside it still count.
    pub fn apply(&mut self, reply: &HeartbeatReply) -> Vec<Event> {
        let received = self.clock.now_ms();
        let checked = reply.validate();
        let mut events: Vec<Event> = checked
            .refused
            .iter()
            .map(|refused| Event::Invalid {
                index: refused.index,
                code: refused.error.code(),
            })
            .collect();
        for command in &checked.commands {
            match command {
                Command::Start(spec) => self.start(spec, received, &mut events),
                Command::Stop { run_id } => self.stop(run_id, false),
                Command::Cancel { run_id } => self.stop(run_id, true),
                Command::Unknown => {}
            }
        }
        if checked.acks.is_ok() {
            reply
                .result_acks
                .iter()
                .for_each(|ack| self.acknowledge(ack));
        }
        self.sweep();
        events
    }

    /// Lets the jobs make progress: watches each running container and its wall-time
    /// limit, then takes each finished job as far through its cleanup as it will go, and
    /// tries again to remove the containers no job stands for that could not be removed.
    /// Call it on every tick.
    pub fn step(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for index in 0..self.jobs.len() {
            self.supervise(index, &mut events);
            self.clean(index, &mut events);
        }
        self.retry_stuck(&mut events);
        self.sweep();
        events
    }

    /// The runs the agent holds, for the next heartbeat: adopted ones first, then in the
    /// order they started. A run is listed until its container and scratch are gone and
    /// its result is acknowledged.
    pub fn held(&self) -> Vec<HeldRun> {
        self.jobs
            .iter()
            .map(|job| HeldRun {
                run_id: job.run_id.clone(),
                state: match job.stage {
                    Stage::Running(_) => HeldState::Running,
                    _ => HeldState::ExitedUnacked,
                },
            })
            .collect()
    }

    /// The results the server has not acknowledged, for the next heartbeat.
    pub fn results(&self) -> Vec<RunResult> {
        self.jobs
            .iter()
            .filter_map(|job| job.result.clone())
            .collect()
    }

    /// What is still free to book.
    pub fn free(&self) -> Capacity {
        let (offered, used) = (&self.config.capacity, self.booked());
        Capacity {
            cpu_millis: offered.cpu_millis.saturating_sub(used.cpu_millis),
            memory_bytes: offered.memory_bytes.saturating_sub(used.memory_bytes),
            gpus: offered.gpus.difference(&used.gpus).copied().collect(),
        }
    }

    /// The findings of the jobs the agent still holds.
    pub fn findings(&self) -> Vec<Finding> {
        self.jobs
            .iter()
            .flat_map(|job| job.findings.iter().cloned())
            .collect()
    }

    /// What the booked slots hold: the sum of the jobs whose slot is not free, and of the
    /// containers that could not be removed.
    fn booked(&self) -> Capacity {
        let mut used = Capacity {
            cpu_millis: 0,
            memory_bytes: 0,
            gpus: BTreeSet::new(),
        };
        let jobs = self.jobs.iter().filter(|job| job.stage != Stage::Freed);
        let bookings = jobs
            .map(|job| &job.booking)
            .chain(self.stuck.iter().map(|held| &held.booking));
        for booking in bookings {
            used.cpu_millis = used.cpu_millis.saturating_add(booking.cpu_millis);
            used.memory_bytes = used.memory_bytes.saturating_add(booking.memory_bytes);
            used.gpus.extend(booking.gpus.iter().copied());
        }
        used
    }

    /// Drops the jobs that hold nothing and owe nothing, remembering their ids.
    fn sweep(&mut self) {
        let gone: Vec<Job> = self
            .jobs
            .extract_if(.., |job| job.stage == Stage::Freed && job.result.is_none())
            .collect();
        gone.iter().for_each(|job| self.retired.insert(&job.run_id));
    }

    fn acknowledge(&mut self, ack: &ResultAck) {
        if ack.ack == Ack::Unknown {
            return;
        }
        self.jobs
            .iter_mut()
            .filter(|job| job.run_id == ack.run_id)
            .for_each(|job| job.result = None);
    }

    fn stop(&mut self, run_id: &str, silent: bool) {
        match self.jobs.iter_mut().find(|job| job.run_id == run_id) {
            Some(job) => job.request_stop(silent),
            None => self.retired.insert(run_id),
        }
    }

    /// Whether a start may run now, and its mounts resolved when it may.
    fn admit(&self, spec: &JobSpec) -> Result<Vec<ResolvedMount>, Refusal> {
        let no = |code: &'static str, why: &str| -> Refusal { (code, why.to_owned()) };
        if (spec.isolation, self.config.allow_host_jobs) == (Isolation::Host, false) {
            return Err(no(
                refusal::HOST_JOBS_NOT_ALLOWED,
                "this agent runs no host jobs",
            ));
        }
        if !spec.gpu_indices.is_empty() && !self.runtime.supports_gpus() {
            return Err(no(
                detail::GPU_UNSUPPORTED,
                "containers on this host cannot use a GPU",
            ));
        }
        let (offered, want) = (&self.config.capacity, &spec.limits);
        let exists = [
            want.cpu_millis <= offered.cpu_millis,
            want.memory_bytes <= offered.memory_bytes,
            spec.gpu_indices
                .iter()
                .all(|gpu| offered.gpus.contains(gpu)),
        ];
        if !exists.into_iter().all(|ok| ok) {
            return Err(no(
                refusal::LIMITS_EXCEED_SHAPE,
                "the job is larger than the machine",
            ));
        }
        let used = self.booked();
        let room = [
            want.cpu_millis <= offered.cpu_millis.saturating_sub(used.cpu_millis),
            want.memory_bytes <= offered.memory_bytes.saturating_sub(used.memory_bytes),
            spec.gpu_indices.iter().all(|gpu| !used.gpus.contains(gpu)),
        ];
        if !room.into_iter().all(|ok| ok) {
            return Err(no(
                refusal::NO_CAPACITY,
                "the job does not fit what is free",
            ));
        }
        spec.mounts
            .iter()
            .map(|mount| self.config.mounts.resolve(mount))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|why| (refusal::INVALID_MOUNT, why.to_string()))
    }

    fn start(&mut self, spec: &JobSpec, received: u64, events: &mut Vec<Event>) {
        let run_id = &spec.run_id;
        let waited = self.clock.now_ms().saturating_sub(received);
        let late = spec.valid_for_ms != 0 && waited > spec.valid_for_ms;
        if late {
            events.push(Event::Skipped {
                run_id: run_id.clone(),
                why: Skip::Stale,
            });
            return;
        }
        if self.jobs.iter().any(|job| &job.run_id == run_id) {
            return;
        }
        if self.retired.contains(run_id) {
            events.push(Event::Skipped {
                run_id: run_id.clone(),
                why: Skip::Retired,
            });
            return;
        }
        if self.jobs.len() >= MAX_JOBS {
            events.push(Event::Skipped {
                run_id: run_id.clone(),
                why: Skip::Full,
            });
            return;
        }
        let mounts = match self.admit(spec) {
            Ok(mounts) => mounts,
            Err((wire, why)) => {
                let mut job = Job::of(spec, Stage::Freed, 0);
                job.result = Some(job.report(Outcome::FailedToStart, None, Some(wire)));
                self.jobs.push(job);
                events.push(Event::Refused {
                    run_id: run_id.clone(),
                    code: wire,
                    why,
                });
                return;
            }
        };
        let started = self.config.scratch.create(run_id).map_err(|why| {
            (
                detail::SCRATCH_FAILED,
                code::SCRATCH_FAILED,
                why.to_string(),
            )
        });
        let started = started.and_then(|scratch| {
            let container = container_spec(spec, mounts, scratch);
            self.runtime
                .create_and_start(&container)
                .map_err(|why| (detail::RUNTIME_FAILED, code::START_FAILED, why.to_string()))
        });
        match started {
            Ok(id) => {
                let deadline = self.clock.now_ms().saturating_add(spec.limits.wall_ms);
                self.jobs.push(Job::of(spec, Stage::Running(id), deadline));
                events.push(Event::Started {
                    run_id: run_id.clone(),
                });
            }
            Err((why, finding, message)) => {
                let mut job = Job::of(spec, Stage::Scratch, 0);
                job.result = Some(job.report(Outcome::FailedToStart, None, Some(why)));
                job.note(finding, message, events);
                events.push(Event::ResultReady {
                    run_id: run_id.clone(),
                });
                self.jobs.push(job);
            }
        }
    }

    /// Watches one running job: the wall-time limit, a stop that is owed, the exit.
    fn supervise(&mut self, index: usize, events: &mut Vec<Event>) {
        let now = self.clock.now_ms();
        let Self {
            jobs,
            runtime,
            config,
            ..
        } = self;
        let job = &mut jobs[index];
        let Stage::Running(id) = job.stage.clone() else {
            return;
        };
        // The exit is looked at first: a job that finished by itself before its limit is
        // reported as it finished, even when the agent looks late. A runtime that cannot
        // say is no reason to let the job run past its limit. A container that is gone has
        // nothing to stop, and the job ends as if it had exited.
        let mut seen = runtime.poll(&id);
        if !matches!(seen, Ok(Status::Exited(_))) {
            if job.cause.is_none() && now >= job.deadline_ms {
                job.cause = Some(Cause::TimedOut);
            }
            let gone = matches!(seen, Ok(Status::Gone));
            if job.cause.is_some() && !job.stop_sent && !gone {
                match runtime.stop(&id, config.stop_grace) {
                    Ok(()) => {
                        job.stop_sent = true;
                        seen = runtime.poll(&id);
                    }
                    Err(why) => job.note(code::STOP_FAILED, why.to_string(), events),
                }
            }
        }
        match seen {
            Ok(Status::Running) => {}
            Ok(ended @ (Status::Exited(_) | Status::Gone)) => {
                let exit = match ended {
                    Status::Exited(exit) => Some(exit),
                    _ => None,
                };
                // What ended the job is what happened, not what the agent meant to do: an
                // exit that came before any stop was sent is the job's own, whether the
                // stop was merely asked for (a `Stop` applied since the last step) or
                // failed (the runtime refused it). A container that is gone has nothing to
                // tell, so the cause stands.
                let cause = job.cause.filter(|_| exit.is_none() || job.stop_sent);
                let (how, status) = outcome(cause, exit);
                if !job.silent {
                    job.result = Some(job.report(how, status, None));
                    events.push(Event::ResultReady {
                        run_id: job.run_id.clone(),
                    });
                }
                job.stage = Stage::Removing(id);
            }
            Err(why) => job.note(code::POLL_FAILED, why.to_string(), events),
        }
    }

    /// Takes one finished job through the cleanup, in order, as far as it goes: the
    /// container, the scratch directory, the checks, then the slot. A step that fails
    /// ends the pass with the slot still booked.
    fn clean(&mut self, index: usize, events: &mut Vec<Event>) {
        let Self {
            jobs,
            runtime,
            config,
            checks,
            ..
        } = self;
        let job = &mut jobs[index];
        let run_id = job.run_id.clone();
        loop {
            match job.stage.clone() {
                Stage::Removing(id) => match runtime.remove(&id) {
                    Ok(()) => {
                        events.push(Event::ContainerRemoved {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Scratch;
                    }
                    Err(why) => {
                        job.note(code::CONTAINER_REMOVE_FAILED, why.to_string(), events);
                        return;
                    }
                },
                Stage::Scratch => match config.scratch.remove(&run_id) {
                    Ok(()) => {
                        events.push(Event::ScratchRemoved {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Checks;
                    }
                    Err(why) => {
                        job.note(code::SCRATCH_FAILED, why.to_string(), events);
                        return;
                    }
                },
                Stage::Checks => match checks.run(&run_id) {
                    Ok(()) => {
                        events.push(Event::ChecksPassed {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Freed;
                        events.push(Event::SlotFreed {
                            run_id: run_id.clone(),
                        });
                    }
                    Err(found) => {
                        // `detail()` is cut, whoever built the finding.
                        let message = match &found {
                            CheckFinding::Residue { .. } => found.detail().to_owned(),
                            CheckFinding::Unchecked { .. } => {
                                format!("the check could not run: {}", found.detail())
                            }
                        };
                        job.note(found.code(), message, events);
                        job.stage = Stage::Blocked;
                        return;
                    }
                },
                Stage::Running(_) | Stage::Blocked | Stage::Freed => return,
            }
        }
    }
}

impl Job {
    /// A job for `spec` in `stage`, booking what the spec asks.
    fn of(spec: &JobSpec, stage: Stage, deadline_ms: u64) -> Self {
        Self {
            run_id: spec.run_id.clone(),
            lease_id: spec.lease_id.clone(),
            booking: Booking {
                cpu_millis: spec.limits.cpu_millis,
                memory_bytes: spec.limits.memory_bytes,
                gpus: spec.gpu_indices.clone(),
            },
            deadline_ms,
            stage,
            cause: None,
            silent: false,
            stop_sent: false,
            result: None,
            findings: Vec::new(),
        }
    }

    /// The job a labelled container stands for, when its labels say what it is.
    fn adopted(found: &Found, now: u64) -> Option<Self> {
        let id = |key: &str| {
            found.labels.get(key).filter(|value| {
                let held = HeldRun {
                    run_id: (*value).clone(),
                    state: HeldState::Running,
                };
                held.validate().is_ok()
            })
        };
        let booking = Booking::from_labels(found)?;
        let wall_ms = label_number(found, label::WALL_MS)?;
        let remaining = wall_ms.saturating_sub(found.running_ms);
        Some(Self {
            run_id: id(label::RUN)?.clone(),
            lease_id: id(label::LEASE)?.clone(),
            booking,
            deadline_ms: now.saturating_add(remaining),
            stage: Stage::Running(found.id.clone()),
            cause: None,
            silent: false,
            stop_sent: false,
            result: None,
            findings: Vec::new(),
        })
    }
}

/// The container of a start: the spec's own labels with the agent's over them.
fn container_spec(
    spec: &JobSpec,
    mounts: Vec<ResolvedMount>,
    scratch: std::path::PathBuf,
) -> ContainerSpec {
    let gpus: Vec<String> = spec.gpu_indices.iter().map(u32::to_string).collect();
    let own = [
        (label::RUN, spec.run_id.clone()),
        (label::LEASE, spec.lease_id.clone()),
        (label::CPU_MILLIS, spec.limits.cpu_millis.to_string()),
        (label::MEMORY_BYTES, spec.limits.memory_bytes.to_string()),
        (label::GPUS, gpus.join(",")),
        (label::WALL_MS, spec.limits.wall_ms.to_string()),
    ];
    let mut labels: BTreeMap<String, String> = spec.labels.clone();
    labels.extend(own.map(|(key, value)| (key.to_owned(), value)));
    ContainerSpec {
        run_id: spec.run_id.clone(),
        image: spec.image.clone(),
        args: spec.args.clone(),
        cpu_millis: spec.limits.cpu_millis,
        memory_bytes: spec.limits.memory_bytes,
        pids: spec.limits.pids,
        gpu_indices: spec.gpu_indices.clone(),
        mounts,
        scratch,
        isolation: spec.isolation,
        labels,
    }
}
