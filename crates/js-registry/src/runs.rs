//! Container runs on registered hosts: what a lease holder queues, what a heartbeat
//! reply carries down, and what the results and the held lists that come back change.
//!
//! The records are in memory only. A restart forgets every one of them, and a host that
//! lists such a run as held is told to cancel it, never adopted. The agent kills a
//! cancelled run without a word: no result comes back, so the run's work is lost and
//! nothing tells the lease holder, who finds the run unknown (see [`crate::Registry`]).
//!
//! A run is bounded by the lease end it was queued with. The bound is kept as an instant
//! and every start is made from the time left at the moment it is sent, so a start that
//! is delivered late carries less time, and none goes out once the time is up.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldRun, HeldState, Isolation, JobSpec, ResultAck,
    RunResult, ValidationError, WireShape, limits,
};
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT};
use js_pool_static::HostBook;
use js_provider::Observed;
use js_types::{HostId, LeaseId, Resources};
use js_usage::ProfileKey;

/// The most runs a host may hold live at once: queued, starting, running or exited but
/// not yet gone from its held list. It keeps the commands of one reply, a start or a stop
/// for each live run, within [`limits::MAX_COMMANDS`] with room for the cancels.
pub const MAX_LIVE_RUNS: usize = 256;

/// The most finished runs kept per host, so a lease holder can still read a result after
/// the run has left the host. The oldest go first: a host that finishes more runs than
/// this under one lease loses the oldest result, and the run is then unknown to the
/// lease.
pub const MAX_FINISHED_RUNS: usize = 256;

const _: () = assert!(
    MAX_LIVE_RUNS < limits::MAX_COMMANDS,
    "a stop for every live run must fit one reply"
);

/// Where a run is, as a lease holder reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Queued for the host; its start goes out on every reply until the host lists the
    /// run.
    Queued,
    /// The host lists the run and is creating its container.
    Starting,
    /// The host lists the run and its container runs.
    Running,
    /// The container has exited and the host still lists the run, until the server has
    /// acknowledged the result. A run that was asked to stop reads `Exited` too once its
    /// result is held.
    Exited,
    /// A stop was asked for and the run is still live: the stop goes out on every reply
    /// until the host no longer lists the run.
    Stopping,
    /// The run has left the host's held list, so its container and scratch are gone and
    /// its limits are back in the lease's budget.
    Ended,
    /// The run never reached the host: the host was lost before it listed the run, or the
    /// run was stopped before the host ever listed it.
    NeverStarted,
}

impl RunState {
    /// The name the consumer API prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Stopping => "stopping",
            Self::Ended => "ended",
            Self::NeverStarted => "never-started",
        }
    }
}

/// One run as the registry knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunView {
    /// The run's id.
    pub run_id: String,
    /// The lease the run belongs to.
    pub lease: LeaseId,
    /// The host it was queued for.
    pub host: HostId,
    /// Where it is.
    pub state: RunState,
    /// The result the host reported and the server accepted, once there is one.
    pub result: Option<RunResult>,
    /// The profile name the lease holder supplied for the work the run does, when it did.
    pub profile: Option<ProfileKey>,
    /// The labels the run was queued with.
    pub labels: BTreeMap<String, String>,
}

/// Why a run was not queued, or not stopped. Each has a constant code
/// ([`RunRefusal::code`]); the text never repeats anything the caller sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunRefusal {
    /// The host is not one that registered itself, so there is no agent to carry a run.
    #[error("the host did not register itself")]
    HostNotRegistered,
    /// The host has not been heard from within the grace period, or has not said hello
    /// since a restart.
    #[error("the host is not heard from")]
    HostLost,
    /// The lease does not hold the host.
    #[error("the lease does not hold the host")]
    NotHeld,
    /// The job breaks a rule of the wire: ids, image, arguments, limits, mounts, labels.
    #[error("{0}")]
    Invalid(ValidationError),
    /// The job's limits, or a GPU index, exceed the shape the lease booked.
    #[error("the job exceeds the booked shape")]
    LimitsExceedShape,
    /// The job asks to change the host, which is refused until the scheduler records the
    /// taint such a job leaves.
    #[error("host jobs are not allowed")]
    HostJobsNotAllowed,
    /// The run does not fit what is left of the lease's budget, the host takes no new
    /// work (out of the serving phase, or marked draining or releasing), or it holds as
    /// many live runs as [`MAX_LIVE_RUNS`] allows.
    #[error("the run does not fit the lease's capacity")]
    NoCapacity,
    /// The host reported that it can no longer be trusted clean, so it takes no new run.
    /// Its live runs still finish.
    #[error("the host is tainted")]
    HostTainted,
    /// The lease is committed to its host and the agent has not yet reported the host
    /// ready (a whole-node lease, whose token is not minted before), so no run is queued.
    #[error("the lease's host is not ready")]
    NotReady,
    /// The run id is already in use. A caller that mints ids never sees it.
    #[error("the run id is in use")]
    RunExists,
    /// The job carries run files and the host did not report the `run-files` capability:
    /// an agent without it would ignore the files and run the job without its secrets.
    #[error("the host does not deliver run files")]
    FilesUnsupported,
    /// The job sets `limits.disk_bytes` and the host did not report the `scratch-quota`
    /// capability: an agent without it would run the job with no disk bound.
    #[error("the host does not bound a run's disk")]
    DiskUnsupported,
    /// The lease has no such run.
    #[error("the lease has no such run")]
    UnknownRun,
}

impl RunRefusal {
    /// The constant code that goes on the wire: the codes of
    /// [`js_agent_wire::refusal`] where one exists.
    pub fn code(&self) -> &'static str {
        use js_agent_wire::refusal;
        match self {
            Self::HostNotRegistered => refusal::HOST_NOT_REGISTERED,
            Self::HostLost => refusal::HOST_LOST,
            Self::NotHeld => refusal::NOT_HELD,
            Self::Invalid(err) => err.code(),
            Self::LimitsExceedShape => refusal::LIMITS_EXCEED_SHAPE,
            Self::HostJobsNotAllowed => refusal::HOST_JOBS_NOT_ALLOWED,
            Self::NoCapacity => refusal::NO_CAPACITY,
            Self::HostTainted => refusal::HOST_TAINTED,
            Self::NotReady => refusal::NOT_READY,
            Self::RunExists => "run-exists",
            Self::FilesUnsupported => "files-unsupported",
            Self::DiskUnsupported => "disk-unsupported",
            Self::UnknownRun => "unknown-run",
        }
    }
}

/// The quota in bytes a host enforces on a run that sets no `disk_bytes`: its
/// `scratch-default-gib` whole GiB, when it reports `scratch-quota` as `project`. `None`
/// when it does not, or when the default is absent, not a whole number, zero, or too
/// large to count in bytes: a host that says it bounds disk and not by how much is
/// treated as one that does not, so the server never books a guess.
pub(crate) fn quota_default(capabilities: &BTreeMap<String, String>) -> Option<u64> {
    let quota = capabilities
        .get(SCRATCH_QUOTA)
        .is_some_and(|value| value == SCRATCH_QUOTA_PROJECT);
    let gib = capabilities
        .get(SCRATCH_DEFAULT_GIB)
        .and_then(|value| value.parse::<u64>().ok());
    let bytes = gib.and_then(|gib| gib.checked_mul(1 << 30));
    bytes.filter(|bytes| [quota, *bytes > 0].into_iter().all(|ok| ok))
}

/// Checks a job against the wire rules and the shape a lease books, before any state is
/// read: the rules, then that it is a contained job, then that its limits and GPU
/// indices fit the shape alone.
pub(crate) fn vet(spec: &JobSpec, shape: &Resources) -> Result<(), RunRefusal> {
    spec.validate().map_err(RunRefusal::Invalid)?;
    spec.require_pinned().map_err(RunRefusal::Invalid)?;
    if spec.isolation == Isolation::Host {
        return Err(RunRefusal::HostJobsNotAllowed);
    }
    let gpus = shape.gpus();
    let indices_fit = spec
        .gpu_indices
        .iter()
        .all(|index| u64::from(*index) < gpus);
    let fits = spec.fits_shape(&WireShape::from_resources(shape)).is_ok();
    match [fits, indices_fit].into_iter().all(|ok| ok) {
        true => Ok(()),
        false => Err(RunRefusal::LimitsExceedShape),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    /// Not yet listed by the host.
    Queued,
    /// Listed by the host, in this state.
    Held(HeldState),
    /// Was listed and no longer is.
    Ended,
    /// Never listed, and will not be.
    NeverStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// No stop was asked for.
    No,
    /// A stop was asked for and no reply has carried it yet.
    Wanted,
    /// A reply carried the stop; it goes out again on every reply while the run is live.
    Sent,
}

#[derive(Debug)]
struct Run {
    host: HostId,
    lease: LeaseId,
    spec: JobSpec,
    profile: Option<ProfileKey>,
    life: Life,
    stop: Stop,
    result: Option<RunResult>,
    order: u64,
    /// The bytes of scratch disk the run was booked at `admit`: its `limits.disk_bytes`, or
    /// the host's default quota at that time when it set none, or zero when the pool books
    /// no disk. Fixed for the life of the run, so a later hello that changes the host's
    /// default does not change what a live run holds.
    disk: u64,
    /// The instant (milliseconds on the registry's clock) the run's bound ends: the lease
    /// end it was queued with. `None` for a run queued without a bound.
    ends: Option<u64>,
}

impl Run {
    /// Whether the run still holds its limits: it is queued, or the host still lists it.
    fn live(&self) -> bool {
        matches!(self.life, Life::Queued | Life::Held(_))
    }

    fn ask_stop(&mut self) {
        if let (Life::Queued | Life::Held(_), Stop::No) = (self.life, self.stop) {
            self.stop = Stop::Wanted;
            // A stopped run's start is never sent, so its files are not kept.
            self.spec.files = Vec::new();
        }
    }

    fn state(&self) -> RunState {
        match (self.life, self.stop, self.result.is_some()) {
            (Life::Held(HeldState::ExitedUnacked), _, true) => RunState::Exited,
            (Life::Queued | Life::Held(_), Stop::Wanted | Stop::Sent, _) => RunState::Stopping,
            (Life::Queued, Stop::No, _) => RunState::Queued,
            (Life::Held(HeldState::Starting), Stop::No, _) => RunState::Starting,
            (Life::Held(HeldState::ExitedUnacked), Stop::No, _) => RunState::Exited,
            (Life::Held(HeldState::Running | HeldState::Unknown), Stop::No, _) => RunState::Running,
            (Life::Ended, _, _) => RunState::Ended,
            (Life::NeverStarted, _, _) => RunState::NeverStarted,
        }
    }

    /// Whether the run was queued with a bound that has passed without the host listing
    /// it: the run can no longer be started, so it is stopped (the stop retires its id on
    /// the host) and then given up when the host still does not list it.
    fn lapsed(&self, now: u64) -> bool {
        let unlisted = self.life == Life::Queued;
        let past = self.ends.is_some_and(|ends| now >= ends);
        [unlisted, past].into_iter().all(|ok| ok)
    }

    /// The start for this run as it can be acted on at `now`: the lease time left cuts
    /// both how long the agent may still start it for and how long it may run. Only for a
    /// run that has not [lapsed](Run::lapsed), which has time left; a bound of zero would
    /// read as no bound on the agent.
    fn start_at(&self, now: u64) -> JobSpec {
        let mut spec = self.spec.clone();
        if let Some(ends) = self.ends {
            let left = ends.saturating_sub(now);
            spec.valid_for_ms = left;
            spec.limits.wall_ms = spec.limits.wall_ms.min(left);
        }
        spec
    }

    fn view(&self) -> RunView {
        RunView {
            run_id: self.spec.run_id.clone(),
            lease: self.lease,
            host: self.host.clone(),
            state: self.state(),
            result: self.result.clone(),
            profile: self.profile.clone(),
            labels: self.spec.labels.clone(),
        }
    }

    /// The verdict on a result of this run from the host that holds the record.
    fn take(&mut self, result: &RunResult, book: &HostBook) -> Ack {
        if self.lease.to_string() != result.lease_id {
            return Ack::Mismatch;
        }
        if self.result.is_some() {
            return Ack::Duplicate;
        }
        if !self.holds(book) {
            return Ack::Stale;
        }
        self.result = Some(result.clone());
        Ack::Accepted
    }

    /// Whether the run's lease still holds the run's host.
    fn holds(&self, book: &HostBook) -> bool {
        let here = Observed::Present {
            host: self.host.clone(),
        };
        book.read(&self.lease) == here
    }
}

/// What a host listing says about a run the server holds: listed means it is on the host;
/// a queued run that is not listed is given up on when the host was lost in between or a
/// stop for it has already gone out, since either way it will never be listed.
fn next_life(life: Life, listed: Option<HeldState>, abandoned: bool) -> Life {
    match (life, listed) {
        (Life::Queued | Life::Held(_), Some(state)) => Life::Held(state),
        (Life::Held(_), None) => Life::Ended,
        (Life::Queued, None) if abandoned => Life::NeverStarted,
        (other, _) => other,
    }
}

/// What a heartbeat's host stands at, as far as sending it a start goes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Standing {
    /// The host was lost or only awaited before this heartbeat.
    pub(crate) gap: bool,
    /// The host reported a taint.
    pub(crate) tainted: bool,
    /// A whole-node lease is committed to the host and the agent has not reported ready.
    pub(crate) unready: bool,
}

/// Every run the server knows, across hosts. The id is the key, so a result for a run of
/// another host is found and told apart from one for no run at all.
#[derive(Debug, Default)]
pub(crate) struct Runs {
    map: BTreeMap<String, Run>,
    next_order: u64,
}

impl Runs {
    fn on<'a>(&'a self, host: &'a HostId) -> impl Iterator<Item = &'a Run> {
        self.map.values().filter(move |run| run.host == *host)
    }

    /// Whether any run of `host` still holds its limits.
    pub(crate) fn live_on(&self, host: &HostId) -> bool {
        self.on(host).any(Run::live)
    }

    /// Forgets every run of `host`.
    pub(crate) fn forget(&mut self, host: &HostId) {
        self.map.retain(|_, run| run.host != *host);
    }

    /// Queues `spec` as a run of `lease` on `host`, which has taken `beats` heartbeats in
    /// its session, at `now`. The caller has vetted the job and checked the host and the
    /// lease; this checks what is left of the lease's budget: the live runs of the host,
    /// their limits, their booked disk and their GPU indices together with this run's must
    /// fit `shape`. `quota_default` is the quota the host enforces on a run that sets no
    /// `disk_bytes`, when it reported one.
    ///
    /// A `spec.valid_for_ms` above zero is the time the lease has left: the run ends its
    /// bound that long after `now`. `profile` is the name the lease holder supplied for the
    /// run's work; the registry keeps it beside the run and never sends it to the host.
    pub(crate) fn admit(
        &mut self,
        host: &HostId,
        lease: &LeaseId,
        (mut spec, profile): (JobSpec, Option<ProfileKey>),
        shape: &Resources,
        quota_default: Option<u64>,
        (beats, now): (u64, u64),
    ) -> Result<RunView, RunRefusal> {
        if self.map.contains_key(&spec.run_id) {
            return Err(RunRefusal::RunExists);
        }
        let live = self.on(host).filter(|run| run.live()).collect::<Vec<_>>();
        let limits = &spec.limits;
        let cpu = live.iter().fold(limits.cpu_millis, |sum, run| {
            sum.saturating_add(run.spec.limits.cpu_millis)
        });
        let memory = live.iter().fold(limits.memory_bytes, |sum, run| {
            sum.saturating_add(run.spec.limits.memory_bytes)
        });
        // A pool that books no disk (shape disk zero) books none for any run; otherwise a
        // run books what it asks for, or the host's default quota when it asks for none
        // (a host with no quota has no default, and books none).
        let disk = match shape.disk_bytes() {
            0 => 0,
            _ => limits.disk_bytes.or(quota_default).unwrap_or(0),
        };
        let disk_sum = live
            .iter()
            .fold(disk, |sum, run| sum.saturating_add(run.disk));
        let taken = live.iter().flat_map(|run| run.spec.gpu_indices.iter());
        let taken = taken.collect::<BTreeSet<_>>();
        let clash = spec.gpu_indices.iter().any(|index| taken.contains(index));
        let room = [
            live.len() < MAX_LIVE_RUNS,
            cpu <= shape.cpu_millis(),
            memory <= shape.memory_bytes(),
            disk_sum <= shape.disk_bytes(),
            !clash,
        ];
        if !room.into_iter().all(|ok| ok) {
            return Err(RunRefusal::NoCapacity);
        }
        spec.heartbeat_seq = beats;
        let bound = Some(spec.valid_for_ms).filter(|bound| *bound > 0);
        let run = Run {
            ends: bound.map(|bound| now.saturating_add(bound)),
            host: host.clone(),
            lease: *lease,
            spec,
            profile,
            life: Life::Queued,
            stop: Stop::No,
            result: None,
            order: self.next_order,
            disk,
        };
        self.next_order = self.next_order.saturating_add(1);
        let view = run.view();
        self.map.insert(run.spec.run_id.clone(), run);
        Ok(view)
    }

    /// The profile name the lease holder supplied for run `run_id`, when the run is known
    /// and was queued with one.
    pub(crate) fn profile_of(&self, run_id: &str) -> Option<ProfileKey> {
        self.map.get(run_id).and_then(|run| run.profile.clone())
    }

    /// Asks every live run of `lease` to stop: the lease ended or is being released.
    pub(crate) fn stop_lease(&mut self, lease: &LeaseId) {
        let runs = self.map.values_mut().filter(|run| run.lease == *lease);
        runs.for_each(Run::ask_stop);
    }

    /// Asks one run of `lease` to stop. A run that already ended is left as it is.
    pub(crate) fn stop_run(
        &mut self,
        lease: &LeaseId,
        run_id: &str,
    ) -> Result<RunView, RunRefusal> {
        let run = self.map.get_mut(run_id);
        let run = run.filter(|run| run.lease == *lease);
        let run = run.ok_or(RunRefusal::UnknownRun)?;
        run.ask_stop();
        Ok(run.view())
    }

    /// The runs of `lease`, oldest first.
    pub(crate) fn of_lease(&self, lease: &LeaseId) -> Vec<RunView> {
        let mut runs = self.map.values().filter(|run| run.lease == *lease);
        let mut runs = runs.by_ref().collect::<Vec<_>>();
        runs.sort_by_key(|run| run.order);
        runs.into_iter().map(Run::view).collect()
    }

    /// Takes the list `held` that `host` sent (in a hello when `gap` is set: the host
    /// was lost or only awaited, so a queued run it does not list will never be listed).
    pub(crate) fn sync(&mut self, host: &HostId, held: &[HeldRun], gap: bool) {
        let listed = held.iter().map(|run| (run.run_id.as_str(), run.state));
        let listed = listed.collect::<BTreeMap<_, _>>();
        let runs = self.map.values_mut().filter(|run| run.host == *host);
        for run in runs {
            let state = listed.get(run.spec.run_id.as_str()).copied();
            let abandoned = [gap, run.stop == Stop::Sent].into_iter().any(|set| set);
            run.life = next_life(run.life, state, abandoned);
            // Once the host lists the run (or the run is given up) no start will be
            // sent again, so the secret files are not kept: they are held only until
            // the agent has taken the start.
            if run.life != Life::Queued {
                run.spec.files = Vec::new();
            }
        }
    }

    /// The reply to a heartbeat of `host` received at `now`: its list is taken, each
    /// result is answered, and the commands go out. A tainted host is sent no start, nor
    /// is one whose exclusive lease is not ready ([`Standing::unready`]); its live runs
    /// finish and their results are answered as ever.
    pub(crate) fn answer(
        &mut self,
        host: &HostId,
        heartbeat: &Heartbeat,
        standing: Standing,
        (book, now): (&HostBook, u64),
    ) -> HeartbeatReply {
        self.sync(host, &heartbeat.held, standing.gap);
        let mut result_acks = Vec::new();
        for result in &heartbeat.results {
            let ack = self.ack(host, result, book);
            result_acks.push(ResultAck {
                run_id: result.run_id.clone(),
                ack,
            });
        }
        let serving = heartbeat.phase == js_agent_wire::Phase::Serving;
        let starts = [serving, !standing.tainted, !standing.unready]
            .into_iter()
            .all(|ok| ok);
        let commands = self.commands(host, (starts, now), &heartbeat.held);
        self.sweep(host, book);
        HeartbeatReply {
            commands,
            result_acks,
            taint_ack: false,
            exclusive: None,
        }
    }

    /// The verdict on one result of `host`:
    ///
    /// - a run of another host is `Mismatch`, and so is a run of this host named under
    ///   another lease;
    /// - a result recorded before is `Duplicate`;
    /// - a result of a run whose lease no longer holds the host is `Stale`;
    /// - a result of a run the server holds no record of (it forgot at a restart) is
    ///   `Stale` when the lease holds this host, `Mismatch` when it holds another, and
    ///   `UnknownLease` when no host holds it;
    /// - otherwise `Accepted`.
    fn ack(&mut self, host: &HostId, result: &RunResult, book: &HostBook) -> Ack {
        match self.map.get_mut(&result.run_id) {
            Some(run) if run.host != *host => Ack::Mismatch,
            Some(run) => run.take(result, book),
            None => {
                let booked = book.booked();
                let holder = booked
                    .iter()
                    .find(|(_, lease)| lease.to_string() == result.lease_id);
                match holder {
                    Some((holder, _)) if holder == host => Ack::Stale,
                    Some(_) => Ack::Mismatch,
                    None => Ack::UnknownLease,
                }
            }
        }
    }

    /// The commands for `host`, most urgent first and at most [`limits::MAX_COMMANDS`]:
    /// a stop for each live run a stop was asked for (all of them fit, see
    /// [`MAX_LIVE_RUNS`]), a cancel for each held run that is not a live run of this host,
    /// then a start for each queued run, unless `starts` is false (the host is not serving
    /// or is tainted). Each start is made at `now` from the lease time left, and none goes
    /// for a run whose time is up: that run is stopped instead, since the host will never
    /// list it, and is given up once the stop has gone out.
    fn commands(
        &mut self,
        host: &HostId,
        (starts_ok, now): (bool, u64),
        held: &[HeldRun],
    ) -> Vec<Command> {
        let mut mine = self
            .map
            .values_mut()
            .filter(|run| run.host == *host)
            .filter(|run| run.live())
            .collect::<Vec<_>>();
        mine.sort_by_key(|run| run.order);
        let mut stops = Vec::new();
        let mut starts = Vec::new();
        for run in mine {
            if run.lapsed(now) {
                run.ask_stop();
            }
            match (run.stop, run.life) {
                (Stop::No, Life::Queued) if starts_ok => {
                    starts.push(Command::Start(Box::new(run.start_at(now))));
                }
                (Stop::No, _) => {}
                _ => {
                    run.stop = Stop::Sent;
                    stops.push(Command::Stop {
                        run_id: run.spec.run_id.clone(),
                    });
                }
            }
        }
        let cancels = held
            .iter()
            .filter(|run| !self.is_live_on(host, &run.run_id))
            .map(|run| Command::Cancel {
                run_id: run.run_id.clone(),
            });
        let all = stops.into_iter().chain(cancels).chain(starts);
        all.take(limits::MAX_COMMANDS).collect()
    }

    fn is_live_on(&self, host: &HostId, run_id: &str) -> bool {
        let run = self.map.get(run_id);
        run.filter(|run| run.host == *host).is_some_and(Run::live)
    }

    /// Drops the finished runs of `host` nobody can read any more (their lease no longer
    /// holds the host), then the oldest beyond [`MAX_FINISHED_RUNS`].
    fn sweep(&mut self, host: &HostId, book: &HostBook) {
        let finished = self.on(host).filter(|run| !run.live());
        let mut finished = finished.collect::<Vec<_>>();
        finished.sort_by_key(|run| run.order);
        let (readable, unreadable): (Vec<_>, Vec<_>) =
            finished.into_iter().partition(|run| run.holds(book));
        let excess = readable.len().saturating_sub(MAX_FINISHED_RUNS);
        let doomed = unreadable.iter().chain(readable.iter().take(excess));
        let doomed = doomed.map(|run| run.spec.run_id.clone());
        let doomed = doomed.collect::<Vec<_>>();
        for id in doomed {
            self.map.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_agent_wire::{Limits, Phase};

    fn job() -> JobSpec {
        JobSpec {
            run_id: "r1".into(),
            lease_id: "1-1".into(),
            image:
                "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .into(),
            args: Vec::new(),
            limits: Limits {
                cpu_millis: 1000,
                memory_bytes: 1 << 20,
                disk_bytes: None,
                pids: None,
                wall_ms: 60_000,
            },
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            isolation: Isolation::Contained,
            labels: Default::default(),
            heartbeat_seq: 0,
            files: Vec::new(),
            valid_for_ms: 0,
            bundle: None,
            native: None,
        }
    }

    fn beat() -> Heartbeat {
        Heartbeat {
            host_id: "a".into(),
            epoch: 1,
            session: String::new(),
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
        }
    }

    /// Asserts the starts gate on its own, whatever the registry lets into the queue: a
    /// queued run of a host whose exclusive lease is not ready is sent no start, and the
    /// same run is started once the host is. It catches a start sent to a host that is
    /// still preparing, which the queue's refusal alone would leave to chance.
    #[test]
    fn a_queued_run_waits_for_an_unready_host() {
        let host = HostId::new("a").unwrap();
        let book = HostBook::new();
        let shape = Resources::new(4000, 8 << 30, 0, None).unwrap();
        let mut runs = Runs::default();
        runs.admit(
            &host,
            &LeaseId::new(1, 1),
            (job(), None),
            &shape,
            None,
            (0, 0),
        )
        .unwrap();
        let held = |unready| Standing {
            gap: false,
            tainted: false,
            unready,
        };
        let reply = runs.answer(&host, &beat(), held(true), (&book, 0));
        assert!(reply.commands.is_empty());
        let reply = runs.answer(&host, &beat(), held(false), (&book, 0));
        assert!(matches!(reply.commands[..], [Command::Start(_)]));
    }

    /// Asserts that a run's files are kept while its start can still be resent and dropped
    /// from the stored run once the host lists it: every beat before the listing carries
    /// the start with the files, the beat that lists it does not resend them, and the
    /// stored run holds none afterwards. It catches secrets kept in the registry's memory
    /// for the whole life of the run.
    #[test]
    fn files_are_dropped_once_the_host_lists_the_run() {
        use js_agent_wire::{HeldRun, HeldState, RunFile};
        let host = HostId::new("a").unwrap();
        let (book, shape) = (
            HostBook::new(),
            Resources::new(4000, 8 << 30, 0, None).unwrap(),
        );
        let mut spec = job();
        spec.files = vec![RunFile {
            name: "tok".into(),
            contents: "x".into(),
        }];
        let mut runs = Runs::default();
        runs.admit(
            &host,
            &LeaseId::new(1, 1),
            (spec.clone(), None),
            &shape,
            None,
            (0, 0),
        )
        .unwrap();
        let open = Standing {
            gap: false,
            tainted: false,
            unready: false,
        };
        for _ in 0..2 {
            let reply = runs.answer(&host, &beat(), open, (&book, 0));
            // The whole command is compared, so no arm exists for a reply that is not
            // the start.
            assert_eq!(reply.commands, vec![Command::Start(Box::new(spec.clone()))]);
        }
        assert_eq!(runs.map["r1"].spec.files, spec.files);
        let mut listed = beat();
        listed.held = vec![HeldRun {
            run_id: "r1".into(),
            state: HeldState::Starting,
        }];
        let reply = runs.answer(&host, &listed, open, (&book, 0));
        assert!(reply.commands.is_empty());
        assert!(runs.map["r1"].spec.files.is_empty());
    }

    /// Asserts that a queued run that is stopped before the host lists it drops its files
    /// at once and its start is never sent. It catches secrets kept in memory for a run
    /// that can no longer start.
    #[test]
    fn a_stopped_queued_run_drops_its_files() {
        use js_agent_wire::RunFile;
        let host = HostId::new("a").unwrap();
        let (book, shape) = (
            HostBook::new(),
            Resources::new(4000, 8 << 30, 0, None).unwrap(),
        );
        let mut spec = job();
        spec.files = vec![RunFile {
            name: "tok".into(),
            contents: "x".into(),
        }];
        let lease = LeaseId::new(1, 1);
        let mut runs = Runs::default();
        runs.admit(&host, &lease, (spec, None), &shape, None, (0, 0))
            .unwrap();
        runs.stop_run(&lease, "r1").unwrap();
        assert!(runs.map["r1"].spec.files.is_empty());
        let open = Standing {
            gap: false,
            tainted: false,
            unready: false,
        };
        let reply = runs.answer(&host, &beat(), open, (&book, 0));
        assert!(
            reply
                .commands
                .iter()
                .all(|c| !matches!(c, Command::Start(_))),
            "{:?}",
            reply.commands
        );
    }
}
