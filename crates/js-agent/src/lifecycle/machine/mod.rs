//! The state machine: one [`Job`] per run the agent holds, moved by the commands of a
//! reply ([`Lifecycle::apply`]) and by the passing of time ([`Lifecycle::step`]).
//!
//! The jobs and what they book are in `job`, a restart in `adopt`, and the end of a job
//! (watching its container and the cleanup in order) in `cleanup`.

mod adopt;
mod cleanup;
mod exclusive;
mod job;
mod slots;

use std::collections::BTreeSet;
use std::path::PathBuf;

use js_agent_wire::{
    Ack, Command, HeartbeatReply, HeldRun, HeldState, Isolation, JobSpec, Outcome, ResultAck,
    RunResult, Taint, limits, refusal,
};

pub use self::exclusive::{Quiet, Restored};
use self::job::{Job, Retired, Stage, Stuck, blind_taint, container_spec, host_taint};
pub use self::slots::Slots;
use super::runtime::{ContainerRuntime, ResolvedMount};
use super::taint::TaintState;
use super::{Capacity, CheckHook, Clock, Config, Event, Finding, Skip, code, detail};

/// How many finished or stopped run ids the agent remembers, so a late or repeated start
/// for one of them runs nothing. The oldest is forgotten first; a start for a run older
/// than the last [`RETIRED_CAP`] is the server's to stop sending.
pub const RETIRED_CAP: usize = limits::MAX_HELD;

/// The most jobs the agent holds, each listed in the held runs of a heartbeat. A job
/// carries at most one result, so the results stay within the wire limit as well.
const MAX_JOBS: usize = limits::MAX_HELD;
const _: () = assert!(limits::MAX_HELD <= limits::MAX_RESULTS);

/// A run that is running, as far as its usage is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningLease {
    /// The lease the run belongs to.
    pub lease_id: String,
    /// The container's control group directory, when the runtime could say.
    pub cgroup: Option<PathBuf>,
    /// When the container started, on the agent's monotonic clock in microseconds, to the
    /// millisecond. `None` for an adopted run that started before the clock began.
    pub started_usec: Option<u64>,
    /// The run was taken over from an earlier process of the agent, so the minutes before
    /// the takeover were never seen.
    pub adopted: bool,
}

/// How many slots the agent has: one for each run it can hold. A slot's container user is
/// the base of `--container-user` plus the slot, so the base plus this many must fit in the
/// ids of a user namespace, which the agent checks when it starts.
pub const SLOTS: u32 = MAX_JOBS as u32;

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
    /// The slots the runs hold, and the ones taken out of use.
    slots: Slots,
    taint: TaintState,
    /// The whole-node lease the server told this host to prepare for, and whether the host
    /// has been found quiet for it (see `exclusive`).
    exclusive: Option<exclusive::Hold>,
    /// The leases of the runs taken over at start, until a reply that names a lease is followed: a
    /// restart while a lease was ready (see `exclusive`).
    adopted_leases: Vec<String>,
}

impl Lifecycle {
    /// Acts on a heartbeat reply: validates it as the wire demands, starts what can be
    /// started, stops what is to be stopped and drops the results the server
    /// acknowledged. A command that fails validation is skipped and reported; the commands
    /// beside it still count.
    pub fn apply(&mut self, reply: &HeartbeatReply) -> Vec<Event> {
        let received = self.clock.now_ms();
        self.apply_at(reply, received)
    }

    /// [`Lifecycle::apply`] for a reply that arrived at `received` on the lifecycle's clock
    /// (milliseconds, [`Clock::now_ms`]): a start's `valid_for_ms` is counted from then, so
    /// a reply that waited for a worker busy with a slow runtime call is not given the time
    /// it spent waiting.
    pub fn apply_at(&mut self, reply: &HeartbeatReply, received: u64) -> Vec<Event> {
        let checked = reply.validate();
        let mut events: Vec<Event> = checked
            .refused
            .iter()
            .map(|refused| Event::Invalid {
                index: refused.index,
                code: refused.error.code(),
            })
            .collect();
        // The lease is followed before the commands, so a `Start` in the reply that grants
        // an exclusive lease is judged against it.
        if checked.exclusive.is_ok() {
            self.follow(reply.exclusive.as_deref());
        }
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
        self.taint.ack(reply.taint_ack);
        self.sweep();
        events
    }

    /// The taint the next heartbeat must carry in `taint`: set from the moment the host is
    /// tainted (or found tainted when the agent starts) until the server acknowledges the
    /// report with `taint_ack`. The acknowledgement stops the report and keeps the marker
    /// and the refusal of new starts: nothing but a reimaged host ends a taint.
    pub fn pending_taint(&self) -> Option<Taint> {
        self.taint.pending()
    }

    /// Whether the host is tainted, acknowledged or not.
    pub fn is_tainted(&self) -> bool {
        self.taint.is_tainted()
    }

    /// Lets the jobs make progress: watches each running container and its wall-time
    /// limit, then takes each finished job as far through its cleanup as it will go, and
    /// tries again to remove the containers no job stands for that could not be removed.
    /// Call it on every tick.
    pub fn step(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        // A marker that could not be written is tried again; the failure was noted when it
        // happened, and the agent refuses starts and frees no slot until it is written.
        let _ = self.taint.persist();
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

    /// The runs that are running, one for each lease (a second run of a lease already named
    /// is left out), in the order of [`Lifecycle::held`].
    pub fn running(&self) -> Vec<RunningLease> {
        let mut named = BTreeSet::new();
        let running = self
            .jobs
            .iter()
            .filter(|job| matches!(job.stage, Stage::Running(_)));
        running
            .filter(|job| named.insert(job.lease_id.as_str()))
            .map(|job| RunningLease {
                lease_id: job.lease_id.clone(),
                cgroup: job.cgroup.clone(),
                started_usec: job.started_usec,
                adopted: job.adopted,
            })
            .collect()
    }

    /// Asks the runtime again for the control group of each run that is running, was
    /// started by this process and has none yet; the usage of a lease with no control group
    /// is not sampled, and a runtime that could not say once (a container it had just
    /// started) may say later. Returns one line for each run that is still without, the
    /// first time only. Adopted runs are left alone: they are never sampled.
    pub fn find_cgroups(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for job in &mut self.jobs {
            let Stage::Running(id) = &job.stage else {
                continue;
            };
            if job.adopted || job.cgroup.is_some() {
                continue;
            }
            job.cgroup = self.runtime.cgroup(id);
            if job.cgroup.is_none() && !job.cgroup_missing_told {
                job.cgroup_missing_told = true;
                lines.push(format!(
                    "usage: the control group of run {} is not known; \
                     looking again at every window",
                    job.run_id
                ));
            }
        }
        lines
    }

    /// The results the server has not acknowledged, for the next heartbeat.
    pub fn results(&self) -> Vec<RunResult> {
        self.jobs
            .iter()
            .filter_map(|job| job.result.clone())
            .collect()
    }

    /// The slots held by a run that has not passed its cleanup.
    pub fn busy_slots(&self) -> Vec<u32> {
        self.slots.held()
    }

    /// The slots that take no new run because the cleanup of their last run did not pass.
    pub fn quarantined_slots(&self) -> Vec<u32> {
        self.slots.quarantined()
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
        if self.taint.is_tainted() {
            return Err(no(refusal::HOST_TAINTED, "this host is tainted"));
        }
        if spec.native.is_some() {
            return Err(no(
                refusal::NATIVE_UNSUPPORTED,
                "this agent runs no native jobs",
            ));
        }
        self.admit_exclusive(spec)?;
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
            self.slots.has_room(),
        ];
        if !room.into_iter().all(|ok| ok) {
            return Err(no(
                refusal::NO_CAPACITY,
                "the job does not fit what is free",
            ));
        }
        let mounts = spec
            .mounts
            .iter()
            .map(|mount| self.config.mounts.resolve(mount))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|why| (refusal::INVALID_MOUNT, why.to_string()))?;
        // Last, so that a job refused for another reason costs no call. Before every
        // contained create: the firewall can change between two jobs.
        if spec.isolation == Isolation::Contained {
            self.runtime
                .check_egress()
                .map_err(|why| (detail::EGRESS_UNCHECKED, why.to_string()))?;
        }
        Ok(mounts)
    }

    /// Taints the host for a job that may change it, and writes the marker.
    fn taint_host_job(&mut self, spec: &JobSpec, events: &mut Vec<Event>) -> Result<(), Refusal> {
        match spec.isolation {
            Isolation::Contained => Ok(()),
            Isolation::Host => self
                .taint
                .seal(host_taint(&spec.run_id), events)
                .map_err(|why| (detail::TAINT_FAILED, why.to_string())),
        }
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
        // A job that may change the host taints it before it exists: whatever it does, and
        // however the agent or the host ends, the marker is already there. One whose marker
        // cannot be written is not started.
        let admitted = self
            .admit(spec)
            .and_then(|mounts| self.taint_host_job(spec, events).map(|()| mounts));
        let mounts = match admitted {
            Ok(mounts) => mounts,
            Err((wire, why)) => {
                let mut job = Job::of(spec, Stage::Freed, 0, None);
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
        // On a host no other job is on, what changed since the last check is the host's. A
        // container that could not be removed, or a check that found something, has tainted
        // the host, and `admit` refused this start; so every other job being freed is enough.
        if self.jobs.iter().all(|job| job.stage == Stage::Freed) {
            self.checks.idle();
        }
        // The slot is held from here until the run's cleanup has passed (`clean`); `admit`
        // found one free.
        let slot = self.slots.take();
        let started = self.config.scratch.create(run_id).map_err(|why| {
            (
                detail::SCRATCH_FAILED,
                code::SCRATCH_FAILED,
                why.to_string(),
                false,
            )
        });
        let started = started.and_then(|scratch| {
            let container = container_spec(spec, mounts, scratch, slot);
            self.runtime.create_and_start(&container).map_err(|why| {
                (
                    detail::RUNTIME_FAILED,
                    code::START_FAILED,
                    why.to_string(),
                    why.is_blind(),
                )
            })
        });
        match started {
            Ok(id) => {
                let now = self.clock.now_ms();
                let deadline = now.saturating_add(spec.limits.wall_ms);
                let cgroup = self.runtime.cgroup(&id);
                let mut job = Job::of(spec, Stage::Running(id), deadline, Some(slot));
                job.started_at(now);
                job.cgroup = cgroup;
                self.jobs.push(job);
                events.push(Event::Started {
                    run_id: run_id.clone(),
                });
            }
            Err((why, finding, message, blind)) => {
                let mut job = Job::of(spec, Stage::Scratch, 0, Some(slot));
                // A host the agent cannot see is not known to be checkable: tainted as
                // unchecked, before the finding that follows from it.
                if blind {
                    job.taint(&mut self.taint, blind_taint(run_id), events);
                }
                job.result = Some(job.report(Outcome::FailedToStart, None, Some(why)));
                job.note(finding, message, events);
                events.push(Event::ResultReady {
                    run_id: run_id.clone(),
                });
                self.jobs.push(job);
            }
        }
    }
}
