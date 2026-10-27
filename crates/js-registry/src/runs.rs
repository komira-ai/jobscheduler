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
use js_pool_static::HostBook;
use js_provider::Observed;
use js_types::{HostId, LeaseId, Resources};

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
    /// work in its phase, or it holds as many live runs as [`MAX_LIVE_RUNS`] allows.
    #[error("the run does not fit the lease's capacity")]
    NoCapacity,
    /// The host reported that it can no longer be trusted clean, so it takes no new run.
    /// Its live runs still finish.
    #[error("the host is tainted")]
    HostTainted,
    /// The run id is already in use. A caller that mints ids never sees it.
    #[error("the run id is in use")]
    RunExists,
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
            Self::RunExists => "run-exists",
            Self::UnknownRun => "unknown-run",
        }
    }
}

/// Checks a job against the wire rules and the shape a lease books, before any state is
/// read: the rules, then that it is a contained job, then that its limits and GPU
/// indices fit the shape alone.
pub(crate) fn vet(spec: &JobSpec, shape: &Resources) -> Result<(), RunRefusal> {
    spec.validate().map_err(RunRefusal::Invalid)?;
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
    life: Life,
    stop: Stop,
    result: Option<RunResult>,
    order: u64,
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
    /// their limits and their GPU indices together with this run's must fit `shape`.
    ///
    /// A `spec.valid_for_ms` above zero is the time the lease has left: the run ends its
    /// bound that long after `now`.
    pub(crate) fn admit(
        &mut self,
        host: &HostId,
        lease: &LeaseId,
        mut spec: JobSpec,
        shape: &Resources,
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
        let taken = live.iter().flat_map(|run| run.spec.gpu_indices.iter());
        let taken = taken.collect::<BTreeSet<_>>();
        let clash = spec.gpu_indices.iter().any(|index| taken.contains(index));
        let room = [
            live.len() < MAX_LIVE_RUNS,
            cpu <= shape.cpu_millis(),
            memory <= shape.memory_bytes(),
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
            life: Life::Queued,
            stop: Stop::No,
            result: None,
            order: self.next_order,
        };
        self.next_order = self.next_order.saturating_add(1);
        let view = run.view();
        self.map.insert(run.spec.run_id.clone(), run);
        Ok(view)
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
        }
    }

    /// The reply to a heartbeat of `host` received at `now`: its list is taken, each
    /// result is answered, and the commands go out. A tainted host is sent no start; its
    /// live runs finish and their results are answered as ever.
    pub(crate) fn answer(
        &mut self,
        host: &HostId,
        heartbeat: &Heartbeat,
        (gap, tainted): (bool, bool),
        (book, now): (&HostBook, u64),
    ) -> HeartbeatReply {
        self.sync(host, &heartbeat.held, gap);
        let mut result_acks = Vec::new();
        for result in &heartbeat.results {
            let ack = self.ack(host, result, book);
            result_acks.push(ResultAck {
                run_id: result.run_id.clone(),
                ack,
            });
        }
        let serving = heartbeat.phase == js_agent_wire::Phase::Serving;
        let starts = [serving, !tainted].into_iter().all(|ok| ok);
        let commands = self.commands(host, (starts, now), &heartbeat.held);
        self.sweep(host, book);
        HeartbeatReply {
            commands,
            result_acks,
            taint_ack: false,
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
