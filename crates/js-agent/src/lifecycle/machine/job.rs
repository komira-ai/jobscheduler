//! What the machine keeps for each run: its [`Job`] and the stage it is in, what it books,
//! how it ended, the run ids that must not start again, and the taints and the container
//! spec that follow from a job.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use js_agent_wire::{HeldRun, HeldState, Isolation, JobSpec, Outcome, RunResult, Taint};

use super::{RETIRED_CAP, SLOTS};
use crate::lifecycle::runtime::{ContainerId, ContainerSpec, Exit, Found, ResolvedMount};
use crate::lifecycle::taint::{TaintState, reason};
use crate::lifecycle::{Capacity, CheckFinding, Event, Finding, code, label};

/// Where a job is. The container's id travels with the stages that need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stage {
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

impl Stage {
    /// The container of a job that is still on the host.
    pub(super) fn container(&self) -> Option<&ContainerId> {
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
}

/// What a job holds of the host while its slot is booked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Booking {
    pub(super) cpu_millis: u64,
    pub(super) memory_bytes: u64,
    pub(super) gpus: Vec<u32>,
}

impl Booking {
    /// Whether this booking fits beside `used` in what `offered` has: the CPU and memory
    /// within what is left, and every GPU offered and not used.
    pub(super) fn fits(&self, offered: &Capacity, used: &Capacity) -> bool {
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
            gpus,
        })
    }

    /// All that `offered` has: what is held for a container whose booking is not known.
    pub(super) fn everything(offered: &Capacity) -> Self {
        Self {
            cpu_millis: offered.cpu_millis,
            memory_bytes: offered.memory_bytes,
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
    pub(super) id: ContainerId,
    pub(super) booking: Booking,
    /// The run its labels name, whose scratch directory the container may still use.
    pub(super) run_id: Option<String>,
    /// The slot its labels name: its uid may still be alive, so the slot is quarantined.
    pub(super) slot: Option<u32>,
}

pub(super) struct Job {
    pub(super) run_id: String,
    pub(super) lease_id: String,
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
    /// Taken over from an earlier process of the agent: its first minutes were not seen.
    pub(super) adopted: bool,
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

    /// Asks a running job to stop; a job past its exit has nothing to stop.
    pub(super) fn request_stop(&mut self, silent: bool) {
        if matches!(self.stage, Stage::Running(_)) {
            self.cause.get_or_insert(Cause::Stopped);
            self.silent |= silent;
        }
    }
}

/// The outcome of a container that exited (`Some`) or vanished (`None`), and the exit
/// status when the job's own exit is what ended it.
pub(super) fn outcome(cause: Option<Cause>, exit: Option<Exit>) -> (Outcome, Option<i32>) {
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
pub(super) struct Retired {
    order: VecDeque<String>,
    seen: BTreeSet<String>,
}

impl Retired {
    pub(super) fn insert(&mut self, run_id: &str) {
        if self.seen.insert(run_id.to_owned()) {
            self.order.push_back(run_id.to_owned());
        }
        if self.order.len() > RETIRED_CAP {
            self.order.pop_front().iter().for_each(|old| {
                self.seen.remove(old);
            });
        }
    }

    pub(super) fn contains(&self, run_id: &str) -> bool {
        self.seen.contains(run_id)
    }
}

impl Job {
    /// A job for `spec` in `stage`, booking what the spec asks, in `slot`.
    pub(super) fn of(spec: &JobSpec, stage: Stage, deadline_ms: u64, slot: Option<u32>) -> Self {
        Self {
            run_id: spec.run_id.clone(),
            lease_id: spec.lease_id.clone(),
            booking: Booking {
                cpu_millis: spec.limits.cpu_millis,
                memory_bytes: spec.limits.memory_bytes,
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
            findings: Vec::new(),
            started_usec: None,
            cgroup: None,
            cgroup_missing_told: false,
            adopted: false,
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
            findings: Vec::new(),
            started_usec: now
                .checked_sub(found.running_ms)
                .map(|started| started.saturating_mul(USEC_PER_MS)),
            cgroup: None,
            cgroup_missing_told: false,
            adopted: true,
        })
    }

    /// Marks the job as started `now_ms` on the agent's clock.
    pub(super) fn started_at(&mut self, now_ms: u64) {
        self.started_usec = Some(now_ms.saturating_mul(USEC_PER_MS));
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

/// The container of a start: the spec's own labels with the agent's over them.
pub(super) fn container_spec(
    spec: &JobSpec,
    mounts: Vec<ResolvedMount>,
    scratch: std::path::PathBuf,
    files: Option<std::path::PathBuf>,
    slot: u32,
) -> ContainerSpec {
    let gpus: Vec<String> = spec.gpu_indices.iter().map(u32::to_string).collect();
    let own = [
        (label::RUN, spec.run_id.clone()),
        (label::LEASE, spec.lease_id.clone()),
        (label::CPU_MILLIS, spec.limits.cpu_millis.to_string()),
        (label::MEMORY_BYTES, spec.limits.memory_bytes.to_string()),
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
        files,
        slot,
        isolation: spec.isolation,
        labels,
    }
}
