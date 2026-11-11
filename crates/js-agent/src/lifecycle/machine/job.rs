//! What the machine keeps for each run: its [`Job`] and the stage it is in, what it books,
//! how it ended, the run ids and job fences that must not start again, and the taints and
//! the container spec that follow from a job.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use js_agent_wire::{
    HeldRun, HeldState, HostPort, Isolation, JobSpec, LogReport, NativeSpec, Outcome, RunResult,
    Taint, refusal,
};
use serde::{Deserialize, Serialize};

use super::{RETIRED_CAP, SLOTS};
use crate::lifecycle::runtime::{
    Body, Exit, Found, JobFs, ResolvedMount, RunHandle, RunSpec, Ticket,
};
use crate::lifecycle::taint::{TaintState, reason};
use crate::lifecycle::{Capacity, CheckFinding, Event, Finding, code, detail, label};
use crate::logs::Sink;

/// Where a job is. The container's id travels with the stages that need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stage {
    /// The slot is booked and the start waits for its images, and for the lookups and the
    /// assembly of its file system ([`Phase`]): no files or container exist yet, no scratch
    /// directory either before the assembly, and the wall-time limit has not begun.
    Preparing(Box<Waiting>),
    /// The container runs.
    Running(RunHandle),
    /// The container has exited; it is to be removed.
    Removing(RunHandle),
    /// The scratch directory is to be removed.
    Scratch,
    /// The post-job checks are to run.
    Checks,
    /// A check found something: the slot stays booked.
    Blocked,
    /// The slot is free; the job is kept only for its unacknowledged result.
    Freed,
}

/// What a job start gives its run beyond the run start it stands for; nothing for a run
/// start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Given {
    /// The environment, by name in order ([`RunSpec::env`]).
    pub(super) env: Vec<(String, String)>,
    /// The checked live-log sink ([`RunSpec::log_sink`]).
    pub(super) log_sink: Option<Sink>,
    /// The start runs in a fresh VM ([`js_agent_wire::StartKind::Vm`]): it is admitted by the
    /// VM rules and its run carries [`label::KIND`].
    pub(super) vm: bool,
    /// The host ports the start names ([`RunSpec::host_ports`]), which the admission holds
    /// to the runtime's [`Runtime::host_ports`](crate::lifecycle::Runtime::host_ports).
    pub(super) host_ports: Vec<HostPort>,
    /// The layers and mounts the start names, which the runtime prepares before the
    /// container is made ([`Runtime::resolve_fs`](crate::lifecycle::Runtime::resolve_fs)).
    pub(super) fs: JobFs,
}

/// What a start that waits is waiting for besides its images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// Nothing else: a start with no layer and no mount.
    Images,
    /// The lookups of its host paths.
    Resolving,
    /// The assembly of its file system, once its directories are made: `scratch` is the
    /// run's scratch directory, the lease directory beside it made too.
    Assembling(PathBuf),
}

/// What a start that waits (for its images, or its file system) keeps to finish starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Waiting {
    pub(super) spec: JobSpec,
    /// What a job start gives its run.
    pub(super) given: Given,
    pub(super) mounts: Vec<ResolvedMount>,
    pub(super) slot: u32,
    /// The disk the run books.
    pub(super) booked: u64,
    /// The images still being fetched (the image, and the images of its layers), each with
    /// the fetch the runtime said it waits on.
    pub(super) tickets: Vec<(String, Ticket)>,
    /// What else it waits for.
    pub(super) phase: Phase,
    /// When the lease the start named ends, on the agent's clock (`u64::MAX` when the
    /// start named none): the run's wall time never reaches past it.
    pub(super) lease_end_ms: u64,
}

impl Stage {
    /// The container of a job that is still on the host.
    pub(super) fn container(&self) -> Option<&RunHandle> {
        match self {
            Self::Running(id) | Self::Removing(id) => Some(id),
            _ => None,
        }
    }
}

/// The taint of a failed cleanup step: `code` is the finding's.
pub(super) fn cleanup_taint(run_id: Option<&str>, code: &str) -> Taint {
    Taint {
        reason: reason::CLEANUP_FAILED.to_owned(),
        run_id: run_id.map(str::to_owned),
        check: Some(code.to_owned()),
    }
}

/// The taint of a post-job check that found something or could not run.
pub(super) fn check_taint(run_id: &str, found: &CheckFinding) -> Taint {
    check_taint_of(Some(run_id), found)
}

/// The taint of a check that found something, for a run that may not be known.
pub(super) fn check_taint_of(run_id: Option<&str>, found: &CheckFinding) -> Taint {
    let why = match found {
        CheckFinding::Residue { .. } => reason::RESIDUE,
        CheckFinding::Unchecked { .. } => reason::UNCHECKED,
    };
    Taint {
        reason: why.to_owned(),
        run_id: run_id.map(str::to_owned),
        check: Some(found.code().to_owned()),
    }
}

/// The taint of a start that showed the agent cannot see the host its jobs run on: the
/// host is not known to be checkable.
pub(super) fn blind_taint(run_id: &str) -> Taint {
    Taint {
        reason: reason::UNCHECKED.to_owned(),
        run_id: Some(run_id.to_owned()),
        check: Some(code::HOST_NOT_VISIBLE.to_owned()),
    }
}

/// Why the agent is ending a job. It decides the result only when the agent's stop is what
/// ended the container (or the container is gone); a container that exited before a stop
/// reached it is reported as it exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cause {
    /// A `Stop` or `Cancel`.
    Stopped,
    /// The wall-time limit.
    TimedOut,
    /// The server was not heard for the silence bound (see `silence`).
    Silenced,
    /// A run adopted after a restart could not be offered its host ports again
    /// (`Runtime::host_port_lost`): its result's `input_failure` says `host-port-lost`.
    HostPortLost,
}

/// The `input_failure` of the result `how` of a job the agent meant to end for `cause`:
/// [`refusal::HOST_PORT_LOST`] for a lost host port when the job did not succeed (stopped for
/// it, or failed by itself first, as a job whose port is gone does), none otherwise.
pub(super) fn cause_input_failure(cause: Option<Cause>, how: Outcome) -> Option<String> {
    let lost = cause == Some(Cause::HostPortLost) && how != Outcome::Succeeded;
    lost.then(|| refusal::HOST_PORT_LOST.to_owned())
}

/// The code of an input of the host that failed, for a job's result: the host port it lost
/// ([`cause_input_failure`]) first, then, for a job that did not succeed, what the runtime
/// found unreadable once it ended (`unreadable`, `Runtime::input_failure`).
pub(super) fn input_failure(
    cause: Option<Cause>,
    how: Outcome,
    unreadable: Option<String>,
) -> Option<String> {
    match (cause_input_failure(cause, how), how) {
        (Some(lost), _) => Some(lost),
        (None, Outcome::Succeeded) => None,
        (None, _) => unreadable,
    }
}

/// The detail of the result of a job that `cause` ended: [`detail::SILENCE_STOP`] for a
/// silence, none otherwise.
pub(super) fn cause_detail(cause: Option<Cause>) -> Option<&'static str> {
    (cause == Some(Cause::Silenced)).then_some(detail::SILENCE_STOP)
}

/// What a job is keyed by on the wire, which decides where it is listed: a run (the lease
/// and run messages: `held`, `results`, `result_acks`) or a job (the per-job messages:
/// `jobs`, `jobs[].result`, `job_acks`). A job is listed under its own keying alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Keyed {
    /// Started by a `Start`: its id is a run id and its lease a lease id.
    Run,
    /// Started by a `JobStart`: its id is a job id and its lease the allocation id.
    Job,
}

/// What a job holds of the host while its slot is booked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Booking {
    pub(super) cpu_millis: u64,
    pub(super) memory_bytes: u64,
    pub(super) disk_bytes: u64,
    pub(super) gpus: Vec<u32>,
}

impl Booking {
    /// Whether this booking fits beside `used` in what `offered` has: the CPU, memory and
    /// disk within what is left, and every GPU offered and not used.
    pub(super) fn fits(&self, offered: &Capacity, used: &Capacity) -> bool {
        [
            self.cpu_millis <= offered.cpu_millis.saturating_sub(used.cpu_millis),
            self.memory_bytes <= offered.memory_bytes.saturating_sub(used.memory_bytes),
            self.disk_bytes <= offered.disk_bytes.saturating_sub(used.disk_bytes),
            self.gpus
                .iter()
                .all(|gpu| offered.gpus.contains(gpu) && !used.gpus.contains(gpu)),
        ]
        .into_iter()
        .all(|ok| ok)
    }

    /// What the labels of a container say it booked: the CPU, memory and GPU labels are present and readable, the disk label is a number when there is one, and no GPU is
    /// named twice (the agent never writes such a booking).
    pub(super) fn from_labels(found: &Found) -> Option<Self> {
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
            // Absent in a container an earlier agent made: it books no disk, the same 0 a
            // job without `disk_bytes` books now, so a restart adopts it. A value that is
            // there and is not a number is an unreadable booking like any other.
            disk_bytes: match found.labels.get(label::DISK_BYTES) {
                None => 0,
                Some(text) => text.parse().ok()?,
            },
            gpus,
        })
    }

    /// All that `offered` has: what is held for a container whose booking is not known.
    pub(super) fn everything(offered: &Capacity) -> Self {
        Self {
            cpu_millis: offered.cpu_millis,
            memory_bytes: offered.memory_bytes,
            disk_bytes: offered.disk_bytes,
            gpus: offered.gpus.iter().copied().collect(),
        }
    }
}

/// The slot a found container's labels name, when they name one that exists.
pub(super) fn slot_label(found: &Found) -> Option<u32> {
    let slot = label_number(found, label::SLOT)?;
    u32::try_from(slot).ok().filter(|slot| *slot < SLOTS)
}

/// The label `key` of a found container, as a number.
fn label_number(found: &Found, key: &str) -> Option<u64> {
    found.labels.get(key)?.parse().ok()
}

/// A container the agent could not remove: it may still run, so what it booked, or the
/// whole offer when that is not known, stays booked until a removal succeeds.
pub(super) struct Stuck {
    pub(super) id: RunHandle,
    pub(super) booking: Booking,
    /// The run its labels name, whose scratch directory the container may still use.
    pub(super) run_id: Option<String>,
    /// The slot its labels name: its uid may still be alive, so the slot is quarantined.
    pub(super) slot: Option<u32>,
}

pub(super) struct Job {
    /// The run id, or the job id of a job keyed by job.
    pub(super) run_id: String,
    /// The lease id, or the allocation id of a job keyed by job.
    pub(super) lease_id: String,
    /// Which messages the job is listed and acknowledged in.
    pub(super) keyed: Keyed,
    pub(super) booking: Booking,
    /// The slot the run holds, none for a run that was refused and one taken over from a
    /// container no slot is written on.
    pub(super) slot: Option<u32>,
    /// On the agent's clock.
    pub(super) deadline_ms: u64,
    pub(super) stage: Stage,
    pub(super) cause: Option<Cause>,
    /// A cancelled job reports nothing.
    pub(super) silent: bool,
    pub(super) stop_sent: bool,
    /// The job may change the host (`isolation = host`).
    pub(super) host: bool,
    /// The record that this job's checks are owed is on disk (written before the first
    /// step of its cleanup, removed once the checks have run).
    pub(super) owed: bool,
    pub(super) result: Option<RunResult>,
    /// The code of an input the host provides that failed, which a job keyed by job lists
    /// in its result ([`JobResult::input_failure`](js_agent_wire::JobResult::input_failure)):
    /// set with the result, and kept with it in the state root.
    pub(super) input_failure: Option<String>,
    /// What the runtime's capture saw of each stream, read with the result's tail: a job
    /// keyed by job lists it in its result. `None` from a runtime that uploads no live logs,
    /// for a run without a sink, and for a result read back from the state root.
    pub(super) logs: Option<LogReport>,
    pub(super) findings: Vec<Finding>,
    /// When the container started, on the agent's monotonic clock, in microseconds (the
    /// lifecycle's clock counts milliseconds, so the stamp is a whole number of them). For
    /// an adopted job it is when the runtime says the container began; `None` when that was
    /// before the clock did, which the clock cannot say.
    pub(super) started_usec: Option<u64>,
    /// The container's control group directory, asked of the runtime when the job starts
    /// or is adopted and, while it is not known, again by `Lifecycle::find_cgroups`.
    pub(super) cgroup: Option<PathBuf>,
    /// The operator was told that the control group is not known yet.
    pub(super) cgroup_missing_told: bool,
    /// The host was tainted because the runtime could not name this run's group.
    pub(super) blind_told: bool,
    /// Taken over from an earlier process of the agent: its first minutes were not seen.
    pub(super) adopted: bool,
    /// When the project of the finished run first read usage after its tree was removed,
    /// and when that was last reported (on the agent's clock).
    pub(super) usage_held: Option<(u64, u64)>,
    /// The agent gave up waiting for the usage: the slot is parked, not released, when the
    /// cleanup passes.
    pub(super) slot_parked: bool,
}

/// Microseconds in a millisecond.
const USEC_PER_MS: u64 = 1000;

impl Job {
    pub(super) fn report(
        &self,
        outcome: Outcome,
        exit_code: Option<i32>,
        detail: Option<&str>,
    ) -> RunResult {
        RunResult {
            run_id: self.run_id.clone(),
            lease_id: self.lease_id.clone(),
            outcome,
            exit_code,
            detail: detail.map(str::to_owned),
            log_tail: None,
        }
    }

    /// Records a finding unless the job already has one of this code.
    pub(super) fn note(&mut self, code: &'static str, message: String, events: &mut Vec<Event>) {
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

    /// Taints the host for this job (the first taint stays) and writes the marker. A marker
    /// that cannot be written is a finding of the job; the host is tainted all the same.
    pub(super) fn taint(&mut self, state: &mut TaintState, taint: Taint, events: &mut Vec<Event>) {
        if let Err(why) = state.seal(taint, events) {
            self.note(code::TAINT_WRITE_FAILED, why.to_string(), events);
        }
    }

    /// Ends a job that was stopped before its container was made: killed, and silent (no
    /// result) when it was cancelled. No container was made: the cleanup that follows
    /// discards what the runtime prepared of its file system, removes the directories made
    /// for its assembly, if any, and gives the slot back.
    pub(super) fn end_unstarted(&mut self, events: &mut Vec<Event>) {
        if !self.silent {
            let detail = cause_detail(self.cause);
            self.result = Some(self.report(Outcome::Killed, None, detail));
            events.push(Event::ResultReady {
                run_id: self.run_id.clone(),
            });
        }
        self.stage = Stage::Scratch;
    }

    /// Where the job is, as a heartbeat lists it: starting while it waits to start,
    /// running while its container runs, and exited from then on.
    pub(super) fn held_state(&self) -> HeldState {
        match self.stage {
            Stage::Preparing(_) => HeldState::Starting,
            Stage::Running(_) => HeldState::Running,
            _ => HeldState::ExitedUnacked,
        }
    }

    /// Asks a running job, or one still waiting to start, to stop; a job past its
    /// exit has nothing to stop.
    pub(super) fn request_stop(&mut self, silent: bool) {
        self.request_stop_for(Cause::Stopped, silent);
    }

    /// Stops a job adopted after a restart whose host ports could not be offered again
    /// (`why`), and notes it once: it ends killed, its result's `input_failure` saying
    /// `host-port-lost`, which a job that exits by itself before the stop reaches it keeps
    /// too unless it succeeded.
    pub(super) fn lose_host_ports(&mut self, why: String, events: &mut Vec<Event>) {
        self.request_stop_for(Cause::HostPortLost, false);
        self.note(code::HOST_PORT_LOST, why, events);
    }

    /// Asks a running job, or one still waiting to start, to stop for `cause` (a cause
    /// already set stays); a job past its exit has nothing to stop. Whether it was running
    /// or waiting.
    pub(super) fn request_stop_for(&mut self, cause: Cause, silent: bool) -> bool {
        let live = matches!(self.stage, Stage::Running(_) | Stage::Preparing(_));
        if live {
            self.cause.get_or_insert(cause);
            self.silent |= silent;
        }
        live
    }
}

/// The outcome of a container that exited (`Some`) or vanished (`None`), and the exit
/// status when the job's own exit is what ended it.
pub(super) fn outcome(cause: Option<Cause>, exit: Option<Exit>) -> (Outcome, Option<i32>) {
    match (cause, exit) {
        (Some(Cause::TimedOut), _) => (Outcome::TimedOut, None),
        (Some(Cause::Stopped | Cause::Silenced | Cause::HostPortLost), _) => {
            (Outcome::Killed, None)
        }
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

/// The fences that must never start again, oldest first: each a run id (or job id) with
/// the allocation (lease) id of the job when it is known, and a count of the fences added,
/// which says whether the list has changed since it was last written down. A run start is
/// judged by its id alone ([`Retired::contains`]); a job start by its fence
/// ([`Retired::fenced`]), so the same job under a new allocation is new work.
#[derive(Default)]
pub(super) struct Retired {
    order: VecDeque<(String, Option<String>)>,
    seen: BTreeSet<(String, Option<String>)>,
    added: u64,
}

impl Retired {
    /// Retires every fence of `run_id`: a stop that names no allocation.
    pub(super) fn insert(&mut self, run_id: &str) {
        self.insert_with(run_id, None);
    }

    /// Retires the fence of `run_id` under `allocation`, or every fence of `run_id` when the
    /// allocation is not known. A fence already retired changes nothing.
    pub(super) fn insert_with(&mut self, run_id: &str, allocation: Option<&str>) {
        let fence = (run_id.to_owned(), allocation.map(str::to_owned));
        if self.seen.insert(fence.clone()) {
            self.order.push_back(fence);
            self.added += 1;
        }
        if self.order.len() > RETIRED_CAP {
            self.order.pop_front().iter().for_each(|old| {
                self.seen.remove(old);
            });
        }
    }

    /// How many ids were added in all: it differs between two readings when the list
    /// changed.
    pub(super) fn added(&self) -> u64 {
        self.added
    }

    /// The retired ids with their allocations, oldest first.
    pub(super) fn entries(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.order
            .iter()
            .map(|(id, allocation)| (id.as_str(), allocation.as_deref()))
    }

    /// Whether any fence of `run_id` is retired: how a run start is judged.
    pub(super) fn contains(&self, run_id: &str) -> bool {
        let from: (String, Option<String>) = (run_id.to_owned(), None);
        let first = self.seen.range(from..).next();
        first.is_some_and(|(id, _)| id == run_id)
    }

    /// Whether the fence of `job_id` under `allocation` is retired, by itself or by a stop
    /// of the job that named no allocation: how a job start is judged.
    pub(super) fn fenced(&self, job_id: &str, allocation: &str) -> bool {
        let every: (String, Option<String>) = (job_id.to_owned(), None);
        let this = (job_id.to_owned(), Some(allocation.to_owned()));
        self.seen.contains(&every) || self.seen.contains(&this)
    }
}

impl Job {
    /// A job for `spec` in `stage`, booking what the spec asks and `disk_bytes` of disk (what
    /// `Config::disk_of` says: the spec's own, or the quota's default), in `slot`.
    pub(super) fn of(
        spec: &JobSpec,
        stage: Stage,
        deadline_ms: u64,
        slot: Option<u32>,
        disk_bytes: u64,
    ) -> Self {
        Self {
            run_id: spec.run_id.clone(),
            lease_id: spec.lease_id.clone(),
            keyed: Keyed::Run,
            booking: Booking {
                cpu_millis: spec.limits.cpu_millis,
                memory_bytes: spec.limits.memory_bytes,
                disk_bytes,
                gpus: spec.gpu_indices.clone(),
            },
            slot,
            deadline_ms,
            stage,
            cause: None,
            silent: false,
            stop_sent: false,
            host: spec.isolation == Isolation::Host,
            owed: false,
            result: None,
            input_failure: None,
            logs: None,
            findings: Vec::new(),
            started_usec: None,
            cgroup: None,
            cgroup_missing_told: false,
            blind_told: false,
            adopted: false,
            usage_held: None,
            slot_parked: false,
        }
    }

    /// A job that holds nothing and only owes its `result` (with its `input_failure`), as one
    /// read back from the state root after a restart: its slot is free, so it is only listed until the result is
    /// acknowledged.
    pub(super) fn held(keyed: Keyed, result: RunResult, input_failure: Option<String>) -> Self {
        Self {
            run_id: result.run_id.clone(),
            lease_id: result.lease_id.clone(),
            keyed,
            booking: Booking {
                cpu_millis: 0,
                memory_bytes: 0,
                disk_bytes: 0,
                gpus: Vec::new(),
            },
            slot: None,
            deadline_ms: 0,
            stage: Stage::Freed,
            cause: None,
            silent: false,
            stop_sent: false,
            host: false,
            owed: false,
            result: Some(result),
            input_failure,
            logs: None,
            findings: Vec::new(),
            started_usec: None,
            cgroup: None,
            cgroup_missing_told: false,
            blind_told: false,
            adopted: false,
            usage_held: None,
            slot_parked: false,
        }
    }

    /// The job a labelled container stands for, when its labels say what it is.
    pub(super) fn adopted(found: &Found, now: u64) -> Option<Self> {
        let booking = Booking::from_labels(found)?;
        let wall_ms = label_number(found, label::WALL_MS)?;
        let remaining = wall_ms.saturating_sub(found.running_ms);
        let slot = match found.labels.contains_key(label::SLOT) {
            true => Some(slot_label(found)?),
            false => None,
        };
        Some(Self {
            run_id: label_id(found, label::RUN)?.to_owned(),
            lease_id: label_id(found, label::LEASE)?.to_owned(),
            keyed: keyed_label(found),
            booking,
            slot,
            deadline_ms: now.saturating_add(remaining),
            stage: Stage::Running(found.id.clone()),
            cause: None,
            silent: false,
            stop_sent: false,
            host: may_change_host(found),
            owed: false,
            result: None,
            input_failure: None,
            logs: None,
            findings: Vec::new(),
            started_usec: now
                .checked_sub(found.running_ms)
                .map(|started| started.saturating_mul(USEC_PER_MS)),
            cgroup: None,
            cgroup_missing_told: false,
            blind_told: false,
            adopted: true,
            usage_held: None,
            slot_parked: false,
        })
    }

    /// Marks the job as started `now_ms` on the agent's clock.
    pub(super) fn started_at(&mut self, now_ms: u64) {
        self.started_usec = Some(now_ms.saturating_mul(USEC_PER_MS));
    }
}

/// The value of the keying label of a container a job start made.
const JOB_KEYED_LABEL: &str = "job";

/// How the job of a found container is keyed: by job when the agent labelled it so, and by
/// run otherwise (a container of a run start, or one an earlier agent made).
fn keyed_label(found: &Found) -> Keyed {
    match found.labels.get(label::KEYING).map(String::as_str) {
        Some(JOB_KEYED_LABEL) => Keyed::Job,
        _ => Keyed::Run,
    }
}

/// The value of the isolation label of a container that runs a host job.
const HOST_LABEL: &str = "host";

/// The value of the isolation label of a container that runs a contained job.
const CONTAINED_LABEL: &str = "contained";

/// The label `key` of a found container, when it is an id the wire accepts.
pub(super) fn label_id<'a>(found: &'a Found, key: &str) -> Option<&'a str> {
    found
        .labels
        .get(key)
        .filter(|value| {
            let held = HeldRun {
                run_id: (*value).clone(),
                state: HeldState::Running,
            };
            held.validate().is_ok()
        })
        .map(String::as_str)
}

/// Whether the job of a found container may have changed the host. Only a container the
/// agent labelled `contained` has not: one labelled `host`, one with no isolation label and
/// one with a label the agent never writes are all taken for a host job's (fail closed).
pub(super) fn may_change_host(found: &Found) -> bool {
    found.labels.get(label::ISOLATION).map(String::as_str) != Some(CONTAINED_LABEL)
}

/// The taint of a host job's start.
pub(super) fn host_taint(run_id: &str) -> Taint {
    host_taint_of(Some(run_id))
}

/// The taint of a host job whose run id may not be known.
pub(super) fn host_taint_of(run_id: Option<&str>) -> Taint {
    Taint {
        reason: reason::HOST_JOB.to_owned(),
        run_id: run_id.map(str::to_owned),
        check: None,
    }
}

/// The run of a start: the spec's own labels with the agent's over them, the body the
/// start names, a container's or a native program's, and what a job start gives it (`given`:
/// its environment by name in order, and its live-log sink; nothing for a run start). `made`
/// is what the lifecycle made for the run: its resolved mounts, its scratch directory and its
/// files directory.
pub(super) fn run_spec(
    (spec, keyed): (&JobSpec, Keyed),
    given: &Given,
    (mounts, scratch, files): (Vec<ResolvedMount>, PathBuf, Option<PathBuf>),
    slot: u32,
    disk_bytes: u64,
) -> RunSpec {
    let gpus: Vec<String> = spec.gpu_indices.iter().map(u32::to_string).collect();
    let own = [
        (label::RUN, spec.run_id.clone()),
        (label::LEASE, spec.lease_id.clone()),
        (label::CPU_MILLIS, spec.limits.cpu_millis.to_string()),
        (label::MEMORY_BYTES, spec.limits.memory_bytes.to_string()),
        (label::DISK_BYTES, disk_bytes.to_string()),
        (label::GPUS, gpus.join(",")),
        (label::SLOT, slot.to_string()),
        (label::WALL_MS, spec.limits.wall_ms.to_string()),
        (
            label::ISOLATION,
            match spec.isolation {
                Isolation::Host => HOST_LABEL.to_owned(),
                Isolation::Contained => CONTAINED_LABEL.to_owned(),
            },
        ),
    ];
    let mut labels: BTreeMap<String, String> = spec.labels.clone();
    labels.extend(own.map(|(key, value)| (key.to_owned(), value)));
    if keyed == Keyed::Job {
        labels.insert(label::KEYING.to_owned(), JOB_KEYED_LABEL.to_owned());
    }
    if given.vm {
        labels.insert(label::KIND.to_owned(), label::VM_KIND.to_owned());
    }
    RunSpec {
        run_id: spec.run_id.clone(),
        // A `run` only reaches here on a runtime that runs image runs: admission refused it
        // elsewhere. Its image is the bundle the run is from.
        body: match (&spec.native, &spec.run) {
            (Some(native), _) => Body::Native(native.clone()),
            (None, Some(run)) => Body::Native(NativeSpec::from(run.clone())),
            (None, None) => Body::Container {
                image: spec.image.clone(),
                args: spec.args.clone(),
            },
        },
        env: given.env.clone(),
        cpu_millis: spec.limits.cpu_millis,
        memory_bytes: spec.limits.memory_bytes,
        pids: spec.limits.pids,
        gpu_indices: spec.gpu_indices.clone(),
        mounts,
        scratch,
        files,
        slot,
        isolation: spec.isolation,
        network: spec.network,
        ports: spec.ports.clone(),
        host_ports: given.host_ports.clone(),
        labels,
        bundle: spec
            .bundle
            .clone()
            .or_else(|| spec.run.as_ref().map(|_| spec.image.clone())),
        log_sink: given.log_sink.clone(),
    }
}
