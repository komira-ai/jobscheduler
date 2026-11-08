//! One agent process: the agent behind the wire (the [`FakeAgent`], or the real lifecycle
//! of [`RealAgent`]), the session with the server, and the runs it launched. Everything
//! here is lost when the process dies.
//!
//! The session (the hellos, the heartbeats and their numbering, the answers and what ends
//! a session) is one code for both agents. What differs is behind [`Behind`]: the fake is
//! told each command and acknowledgement one by one, as [`AgentHarness`] calls; the real
//! lifecycle is handed the whole reply and builds the held runs and results of the next
//! heartbeat itself.
//!
//! The whole-host handshake ([`Exclusive`]) is the session's, for both agents: neither
//! knows it. The phase of every hello and heartbeat, its ready report and its refusal come
//! from it; a reply's `exclusive` is handed to it before the reply's commands; and while it
//! prepares or has refused, no start of the reply reaches the agent (the fake is not told
//! of it, the real lifecycle is handed the reply without it). Every start that reaches the
//! process, and every stop or cancel of a run it runs, is written into the cell's ledger
//! first, whichever agent is behind it.
//!
//! The fake's half (what it holds and is told) is in `fake`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent::{BASE, REQUEST_LIMIT};
use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldRun, Hello, JobSpec, ProtocolRange,
    RETRY_AFTER_CAP, ResultAck, RunResult, Welcome, WireShape, refusal,
};
use js_conformance::agent::{AgentHarness, FakeAgent};
use js_jobs::JobId;
use js_types::sched::FarmTime;
use js_types::{LeaseId, Resources};

mod fake;

use super::exclusive::Exclusive;
use super::per_job::{Acks, Listing, PerJob};
use super::real::RealAgent;
use super::real_jobs::RealJobs;
use super::{AgentKind, AgentSpec, Io, SIM_RUN_IMAGE};
use crate::bus::{CallId, Route};
use crate::counters::Counter;
use crate::ledger::{Acked, Completion};
use crate::observe::{
    AgentTruth, Launch, ReadyReport, RunId, Running, RunningJob, StartSeen, StopCommand, StopSeen,
    Verdict,
};

/// The wait before the heartbeat that follows one that had no answer: the shortest wait of
/// the real agent's backoff.
const BACKOFF: Duration = BASE;

/// The agent version every hello names.
const AGENT_VERSION: &str = "0.0.0-sim";

/// An answer to one of the process's calls.
pub(super) struct Answer {
    pub(super) id: CallId,
    pub(super) status: u16,
    pub(super) retry_after: Option<Duration>,
    pub(super) body: String,
}

/// A welcomed session.
struct Session {
    epoch: u64,
    /// The session secret: in every heartbeat body, and nowhere else.
    secret: String,
    interval: Duration,
    /// The number of the last heartbeat sent.
    seq: u64,
    /// The heartbeat sent and not yet answered. The agent has one call in flight at a time
    /// and gives it [`REQUEST_LIMIT`]; an answer to any other call is dropped unread.
    in_flight: Option<CallId>,
    /// Whether a heartbeat of the session was answered 200.
    accepted: bool,
}

impl Session {
    /// Ends the call in flight when `id` is it, noting a 200.
    fn answer(&mut self, id: CallId, status: u16) -> Option<()> {
        let open = self.in_flight.filter(|open| *open == id);
        open.map(|_| {
            self.in_flight = None;
            self.accepted |= status == 200;
        })
    }
}

/// A run the process launched, as its start named it.
struct RunInfo {
    /// The lease id as the start wrote it, repeated in the run's result.
    lease_text: String,
    lease: LeaseId,
    resources: Resources,
    /// The run's wall limit, as its start set it.
    wall: Duration,
    /// When that limit runs out: set at the launch.
    wall_ends_at: FarmTime,
}

impl RunInfo {
    /// The run `job` starts, on a host of `capacity`; `None` when the start names a
    /// lease the cell's server would not write (`<term>-<sequence>`) or GPUs the host has
    /// no class for.
    fn of(job: &JobSpec, capacity: &Resources) -> Option<Self> {
        let limits = &job.limits;
        let gpus = job.gpu_indices.len() as u64;
        let class = capacity.gpu_class().filter(|_| gpus > 0).map(str::to_owned);
        let resources = Resources::new(limits.cpu_millis, limits.memory_bytes, gpus, class)
            .ok()
            .map(|r| r.with_disk_bytes(limits.disk_bytes.unwrap_or(0)));
        lease_id(&job.lease_id)
            .zip(resources)
            .map(|(lease, resources)| RunInfo {
                lease_text: job.lease_id.clone(),
                lease,
                resources,
                wall: Duration::from_millis(job.limits.wall_ms),
                wall_ends_at: FarmTime::default(),
            })
    }
}

/// The lease `text` names in the server's form, `<term>-<sequence>`.
pub(crate) fn lease_id(text: &str) -> Option<LeaseId> {
    text.split_once('-')
        .and_then(|(term, seq)| term.parse::<u64>().ok().zip(seq.parse::<u64>().ok()))
        .map(|(term, seq)| LeaseId::new(term, seq))
}

/// How long a run of `job` takes, when it is a [`SIM_RUN_IMAGE`] run.
fn sim_run(job: &JobSpec) -> Option<Duration> {
    let millis = job.args.first().and_then(|first| first.parse::<u64>().ok());
    millis
        .filter(|_| job.image == SIM_RUN_IMAGE)
        .map(Duration::from_millis)
}

/// The cell's verdict for the wire's, or `None` for one this version cannot read: the
/// agent keeps that result and sends it again.
pub(super) fn verdict(ack: Ack) -> Option<Verdict> {
    match ack {
        Ack::Accepted => Some(Verdict::Accepted),
        Ack::Duplicate => Some(Verdict::Duplicate),
        Ack::Stale => Some(Verdict::Stale),
        Ack::UnknownLease => Some(Verdict::UnknownLease),
        Ack::Mismatch => Some(Verdict::Mismatch),
        Ack::Unknown => None,
    }
}

/// Whether `body` is the API's error body with the code `code`.
fn is_code(body: &str, code: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body).is_ok_and(|value| value["error"] == code)
}

/// The agent behind the wire.
enum Behind {
    /// The conformance suite's fake, told each command one by one.
    Fake(Box<FakeAgent>),
    /// The real lifecycle, handed each reply whole.
    Real(Box<RealAgent>),
}

impl Behind {
    /// The agent `spec` names, fresh; `None` for a real one whose directories or
    /// lifecycle could not be made.
    fn of(spec: &AgentSpec) -> Option<Self> {
        match &spec.kind {
            AgentKind::Fake(Some(defect)) => {
                Some(Behind::Fake(Box::new(FakeAgent::broken(*defect))))
            }
            AgentKind::Fake(None) => Some(Behind::Fake(Box::new(FakeAgent::with_window(None)))),
            AgentKind::Real(root) => RealAgent::new(root, &spec.host, &spec.capacity)
                .map(|real| Behind::Real(Box::new(real))),
        }
    }

    /// The fake, when this is it.
    fn fake(&mut self) -> Option<&mut FakeAgent> {
        match self {
            Behind::Fake(agent) => Some(agent.as_mut()),
            Behind::Real(_) => None,
        }
    }

    /// The real agent, when this is it.
    fn real(&self) -> Option<&RealAgent> {
        match self {
            Behind::Fake(_) => None,
            Behind::Real(real) => Some(real),
        }
    }

    /// The real agent, when this is it.
    fn real_mut(&mut self) -> Option<&mut RealAgent> {
        match self {
            Behind::Fake(_) => None,
            Behind::Real(real) => Some(real),
        }
    }

    /// The runs whose jobs run now.
    fn running(&self) -> BTreeSet<String> {
        match self {
            Behind::Fake(agent) => agent.observed().running,
            Behind::Real(real) => real.running(),
        }
    }

    /// Every run the agent holds: those it runs, and those whose result is not yet
    /// acknowledged (the real lifecycle also lists a run until its cleanup is done).
    fn held(&self) -> Vec<HeldRun> {
        match self {
            Behind::Fake(agent) => fake::held(agent),
            Behind::Real(real) => real.held(),
        }
    }

    /// The results not yet acknowledged, of the runs in `runs`.
    fn results(&self, runs: &BTreeMap<String, RunInfo>) -> Vec<RunResult> {
        match self {
            Behind::Fake(agent) => fake::results(agent, runs),
            Behind::Real(real) => real.results(),
        }
    }
}

/// What a process keeps of the runs it was told about, whichever agent is behind it.
#[derive(Default)]
struct Book {
    /// Every run launched.
    runs: BTreeMap<String, RunInfo>,
    /// The run each armed end timer ends.
    run_timers: BTreeMap<u64, String>,
    /// Every run a stop or cancel named.
    stopped: BTreeSet<String>,
    /// Every run whose result was sent at least once.
    reported: BTreeSet<String>,
    /// Every run a hello or heartbeat of the process listed as held: what the server can
    /// have seen listed.
    listed: BTreeSet<String>,
    completed: BTreeMap<RunId, FarmTime>,
    acknowledged: BTreeMap<RunId, Verdict>,
}

impl Book {
    /// Counts a start of `run` that arrives after a stop of it, or for a run the process
    /// launched already.
    fn note_start(&self, run: &str, io: &mut Io) {
        if self.stopped.contains(run) {
            io.bump(Counter::StartAfterStop);
        } else if self.runs.contains_key(run) {
            io.bump(Counter::StartRepeated);
        }
    }

    /// The agent launched `job`: the ledger is told, and a [`SIM_RUN_IMAGE`] run's end is
    /// armed.
    fn launched(&mut self, job: &JobSpec, mut info: RunInfo, io: &mut Io) {
        info.wall_ends_at = io.now.saturating_add(info.wall);
        let run = RunId::new(job.run_id.clone());
        io.ledger.launched(Launch {
            run,
            host: io.host.clone(),
            at: io.now,
        });
        if let Some(after) = sim_run(job) {
            let tag = io.timer(after);
            self.run_timers.insert(tag, job.run_id.clone());
        }
        self.runs.insert(job.run_id.clone(), info);
    }

    /// `run` completed by itself, now.
    fn completed(&mut self, run: String, io: &mut Io) {
        let run = RunId::new(run);
        self.completed.insert(run.clone(), io.now);
        io.ledger.completed(Completion {
            run,
            host: io.host.clone(),
            at: io.now,
        });
    }

    /// Counts each of `results` that was sent before.
    fn count_resent(&mut self, results: &[RunResult], io: &mut Io) {
        for result in results {
            if !self.reported.insert(result.run_id.clone()) {
                io.bump(Counter::ResultResent);
            }
        }
    }
}

/// One agent process.
pub(super) struct Process {
    agent: Behind,
    session: Option<Session>,
    /// The hellos not yet answered, with when each was sent.
    hellos: BTreeMap<CallId, FarmTime>,
    /// When each heartbeat of the session was sent, by number; 0 is the hello the
    /// session's welcome answered.
    beats_sent: BTreeMap<u64, FarmTime>,
    book: Book,
    /// Whether the agent was told its connection dropped and not yet that it is back.
    disconnected: bool,
    /// Where the process is in the whole-host handshake.
    exclusive: Exclusive,
    /// The per-job half, for a fake agent that speaks the per-job messages.
    jobs: Option<PerJob>,
    /// The per-job half, for a real agent that speaks the per-job messages.
    real_jobs: Option<RealJobs>,
    /// How long the worker takes to act on a reply ([`AgentSpec::worker_lag`]).
    lag: Duration,
    /// When the worker is done with the replies queued so far.
    busy_until: FarmTime,
    /// The replies read and not yet acted on, by the tag of the timer that ends their wait:
    /// the call they answer and their body.
    queued: BTreeMap<u64, (CallId, String)>,
}

impl Process {
    /// A fresh process: the agent `spec` names, holding nothing, not welcomed; `None` for
    /// a real agent that could not be made.
    pub(super) fn new(spec: &AgentSpec) -> Option<Self> {
        Behind::of(spec).map(|agent| Self {
            agent,
            session: None,
            hellos: BTreeMap::new(),
            beats_sent: BTreeMap::new(),
            book: Book::default(),
            disconnected: false,
            exclusive: Exclusive::default(),
            jobs: PerJob::of(spec),
            real_jobs: RealJobs::of(spec),
            lag: spec.worker_lag,
            busy_until: FarmTime::default(),
            queued: BTreeMap::new(),
        })
    }

    /// The time is `now`: the real lifecycle's clock reads it from here on.
    pub(super) fn at(&self, now: FarmTime) {
        if let Behind::Real(real) = &self.agent {
            real.at(now);
        }
    }

    /// The process's ground truth, on a host of `capacity`.
    pub(super) fn truth(&self, capacity: &Resources) -> AgentTruth {
        let running = self.agent.running().into_iter().filter_map(|run| {
            self.book.runs.get(&run).map(|info| {
                let running = Running {
                    lease: info.lease,
                    resources: info.resources.clone(),
                };
                (RunId::new(run), running)
            })
        });
        AgentTruth {
            capacity: capacity.clone(),
            running: running.collect(),
            completed: self.book.completed.clone(),
            acknowledged: self.book.acknowledged.clone(),
            jobs: self.jobs_truth(),
        }
    }

    /// The jobs the agent runs now, whichever agent is behind the per-job half.
    fn jobs_truth(&self) -> BTreeMap<JobId, RunningJob> {
        let fake = self.jobs.as_ref().map(PerJob::truth);
        let jobs = self.real_jobs.as_ref().zip(self.agent.real());
        let real = jobs.map(|(jobs, real)| jobs.truth(real));
        fake.or(real).unwrap_or_default()
    }

    /// What the next heartbeat lists as jobs, whichever agent is behind the per-job half.
    fn jobs_listing(&self) -> Listing {
        let fake = self.jobs.as_ref().map(PerJob::listing);
        let real = self.real_jobs.as_ref().zip(self.agent.real());
        let real = real.map(|(_, real)| RealJobs::listing(real));
        fake.or(real).unwrap_or_default()
    }

    /// The process's tick: a hello while not welcomed, else a heartbeat. Returns when the
    /// next tick is due.
    pub(super) fn tick(&mut self, spec: &AgentSpec, io: &mut Io) -> Duration {
        let Some(mut session) = self.session.take() else {
            self.hello(spec, io);
            return spec.hello_interval;
        };
        // A tick with the heartbeat still in flight is the end of its time: the exchange
        // failed, and its answer, should it come, is dropped. The next heartbeat follows
        // after the shortest wait of the agent's backoff.
        let next = if session.in_flight.take().is_some() {
            self.timed_out();
            BACKOFF
        } else {
            self.beat(&mut session, spec, io);
            REQUEST_LIMIT
        };
        self.session = Some(session);
        next
    }

    /// A timer of the process fired: the end of a run, when the tag is one. The run
    /// completes unless it no longer runs (it was stopped, or this is the end of a second
    /// launch of a run that already ended).
    pub(super) fn timer(&mut self, tag: u64, capacity: &Resources, io: &mut Io) {
        if let Some((id, body)) = self.queued.remove(&tag) {
            return self.act(id, &body, capacity, io);
        }
        let fake = self.jobs.as_mut().is_some_and(|jobs| jobs.timer(tag, io));
        let real = self.real_jobs.as_mut().zip(self.agent.real_mut());
        let real = real.is_some_and(|(jobs, real)| jobs.timer(tag, real, io));
        if fake || real {
            return;
        }
        let Some(run) = self.book.run_timers.remove(&tag) else {
            return;
        };
        let ended = match &mut self.agent {
            Behind::Fake(agent) => fake::run_ends(agent, &run),
            Behind::Real(real) => real.run_ends(&run),
        };
        if ended {
            self.book.completed(run, io);
        }
    }

    /// An answer arrived. An answer to no open call (a hello already answered, a heartbeat
    /// of an ended session) is ignored.
    pub(super) fn answer(&mut self, answer: Answer, spec: &AgentSpec, io: &mut Io) {
        if let Some(sent) = self.hellos.remove(&answer.id) {
            self.reconnect();
            return self.hello_answered(sent, answer, spec, io);
        }
        let beat = self
            .session
            .as_mut()
            .and_then(|session| session.answer(answer.id, answer.status));
        if beat.is_some() {
            self.reconnect();
            self.beat_answered(answer, spec, io);
            // The next heartbeat is an interval after the answer, unless the session ended.
            let interval = self.session.as_ref().map(|session| session.interval);
            interval.into_iter().for_each(|wait| io.arm_tick(wait));
        }
    }

    fn hello(&mut self, spec: &AgentSpec, io: &mut Io) {
        let hello = Hello {
            protocol: ProtocolRange::THIS,
            host_id: spec.host.as_str().to_owned(),
            pool: spec.pool.clone(),
            agent_version: AGENT_VERSION.to_owned(),
            instance: spec.instance.clone(),
            shape: WireShape::from_resources(&spec.capacity),
            capabilities: spec.capabilities.clone(),
            gpu_devices: Vec::new(),
            labels: spec.labels.clone(),
            phase: self.exclusive.phase(),
            held: self.list(),
            accepts_host_credential: false,
            identity: None,
        };
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&hello).unwrap_or_default();
        let id = io.call(Route::Hello, &spec.join_token, body);
        self.hellos.insert(id, io.now);
    }

    /// Tells the agent its connection dropped: the heartbeat had no answer in
    /// [`REQUEST_LIMIT`]. The real lifecycle has no such call: it learns nothing from a
    /// lost answer.
    fn timed_out(&mut self) {
        if let Some(agent) = self.agent.fake() {
            agent.disconnect();
        }
        self.jobs.iter_mut().for_each(PerJob::disconnect);
        self.disconnected = true;
    }

    /// Sends the next heartbeat. The real lifecycle takes a step first, as the real agent
    /// does on each tick, so the body carries what it found.
    fn beat(&mut self, session: &mut Session, spec: &AgentSpec, io: &mut Io) {
        if let Behind::Real(real) = &mut self.agent {
            real.step();
        }
        self.settle(io);
        session.seq += 1;
        self.beats_sent.insert(session.seq, io.now);
        let results = self.agent.results(&self.book.runs);
        self.book.count_resent(&results, io);
        let held = self.list();
        let listing = self.jobs_listing();
        let (ready, refused) = self.exclusive.report(io.prepare, held.is_empty(), io.now);
        if let Some(lease) = &ready {
            io.ledger.reported_ready(ReadyReport {
                host: io.host.clone(),
                lease: lease.clone(),
                at: io.now,
            });
        }
        let beat = Heartbeat {
            host_id: spec.host.as_str().to_owned(),
            epoch: session.epoch,
            session: session.secret.clone(),
            phase: self.exclusive.phase(),
            held,
            results,
            taint: None,
            exclusive_ready: ready,
            exclusive_refused: refused,
            load: None,
            disk_gib: None,
            jobs: listing.held.clone(),
            retired_jobs: listing.retired.clone(),
            log_grant_state: None,
        };
        if let Some(agent) = self.agent.fake() {
            agent.heartbeat();
        }
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&beat).unwrap_or_default();
        let id = io.call(Route::Heartbeat, &spec.join_token, body);
        session.in_flight = Some(id);
        if let Some(jobs) = self.jobs.as_mut() {
            jobs.sent(id, listing.clone(), io);
        }
        if let Some(jobs) = self.real_jobs.as_mut() {
            jobs.sent(id, listing, io);
        }
    }

    /// Writes down the results the real lifecycle holds now, when it speaks the per-job
    /// messages.
    fn settle(&mut self, io: &mut Io) {
        let jobs = self.real_jobs.as_mut().zip(self.agent.real());
        jobs.into_iter()
            .for_each(|(jobs, real)| jobs.settle(real, io));
    }

    /// What a hello or heartbeat lists as held ([`Behind::held`]), noted as listed.
    fn list(&mut self) -> Vec<HeldRun> {
        let held = self.agent.held();
        let names = held.iter().map(|run| run.run_id.clone());
        self.book.listed.extend(names);
        held
    }

    /// Whether the process runs a run that no hello or heartbeat it sent listed, so the
    /// server cannot have seen it listed.
    pub(super) fn runs_unlisted(&self) -> bool {
        let running = self.agent.running();
        let runs = running.iter().any(|run| !self.book.listed.contains(run));
        let fake = self.jobs.as_ref().is_some_and(PerJob::runs_unlisted);
        let real = self.real_jobs.as_ref().zip(self.agent.real());
        let real = real.is_some_and(|(jobs, real)| jobs.runs_unlisted(real));
        runs || fake || real
    }

    fn reconnect(&mut self) {
        if self.disconnected {
            self.disconnected = false;
            if let Some(agent) = self.agent.fake() {
                agent.reconnect();
            }
            self.jobs.iter_mut().for_each(PerJob::reconnect);
        }
    }

    fn hello_answered(&mut self, sent: FarmTime, answer: Answer, spec: &AgentSpec, io: &mut Io) {
        match answer.status {
            200 => self.welcomed(sent, &answer.body, io),
            429 => {
                let wait = answer.retry_after.unwrap_or(spec.hello_interval);
                io.arm_tick(wait.min(RETRY_AFTER_CAP));
            }
            _ => {}
        }
    }

    fn welcomed(&mut self, sent: FarmTime, body: &str, io: &mut Io) {
        let welcome = serde_json::from_str::<Welcome>(body)
            .ok()
            .filter(|welcome| welcome.validate().is_ok());
        let Some(welcome) = welcome else {
            return io.bump(Counter::ReplyInvalid);
        };
        let interval = Duration::from_millis(welcome.heartbeat_interval_ms);
        self.hellos.clear();
        self.beats_sent = BTreeMap::from([(0, sent)]);
        self.session = Some(Session {
            epoch: welcome.epoch,
            secret: welcome.session,
            interval,
            seq: 0,
            in_flight: None,
            accepted: false,
        });
        io.arm_tick(interval);
    }

    fn beat_answered(&mut self, answer: Answer, spec: &AgentSpec, io: &mut Io) {
        match answer.status {
            200 => self.read(answer.id, answer.body, &spec.capacity, io),
            409 => {
                if is_code(&answer.body, refusal::UNKNOWN_HOST) {
                    io.bump(Counter::UnknownHost);
                }
                self.session_lost(spec, io);
            }
            403 if is_code(&answer.body, refusal::SESSION_MISMATCH) => self.session_lost(spec, io),
            _ => {}
        }
    }

    /// The server no longer holds the session: hello again, at once when a heartbeat of
    /// the session was accepted, else at the next tick.
    fn session_lost(&mut self, spec: &AgentSpec, io: &mut Io) {
        let accepted = self.session.take().is_some_and(|session| session.accepted);
        if accepted {
            self.hello(spec, io);
        }
        io.arm_tick(spec.hello_interval);
    }

    /// Hands a 200 reply to the worker: it acts on it now when it has no lag, else once it
    /// is done with the replies before this one and has taken its lag.
    fn read(&mut self, id: CallId, body: String, capacity: &Resources, io: &mut Io) {
        if self.lag.is_zero() {
            return self.act(id, &body, capacity, io);
        }
        let start = self.busy_until.max(io.now);
        self.busy_until = start.saturating_add(self.lag);
        let wait = self.busy_until.as_millis() - io.now.as_millis();
        let tag = io.timer(Duration::from_millis(wait));
        self.queued.insert(tag, (id, body));
    }

    /// Acts on a heartbeat reply: its `exclusive` first, when it passes the wire's check,
    /// then every command and acknowledgement that passes the wire's checks, after the
    /// ledger is told of each start and of each stop or cancel of a running run. The fake
    /// is told each; the real lifecycle is handed the reply, with the starts the cell
    /// cannot account for taken out. While the agent prepares or has refused, no start
    /// reaches either. The per-job half, when the agent speaks the per-job messages, is
    /// handed the reply last, as the answer to the heartbeat sent as call `id`.
    fn act(&mut self, id: CallId, body: &str, capacity: &Resources, io: &mut Io) {
        let Ok(mut reply) = serde_json::from_str::<HeartbeatReply>(body) else {
            return io.bump(Counter::ReplyInvalid);
        };
        let check = reply.validate();
        if !check.refused.is_empty() || check.acks.is_err() || check.exclusive.is_err() {
            io.bump(Counter::ReplyInvalid);
        }
        if check.acks.is_ok() {
            note_verdicts(&reply.result_acks, io);
        }
        if check.exclusive.is_ok() {
            self.exclusive.told(reply.exclusive.as_deref(), io.now);
        }
        let takes = self.exclusive.takes_starts();
        self.seen(&check.commands, capacity, io);
        match &mut self.agent {
            Behind::Fake(agent) => {
                agent.contact();
                let handed = check.commands.iter().filter(|c| takes || !is_start(c));
                for command in handed {
                    let book = &mut self.book;
                    fake::command(agent, book, &self.beats_sent, command, capacity, io);
                }
                if check.acks.is_ok() {
                    for ack in &reply.result_acks {
                        fake::ack(agent, &mut self.book, ack);
                    }
                }
            }
            Behind::Real(real) => {
                let handed = check.commands.iter().copied();
                let handed: Vec<&Command> = handed.filter(|c| takes || !is_start(c)).collect();
                let mut starts = self.book.starts(&handed, capacity, io);
                let withheld = check.commands.iter().filter_map(|command| match command {
                    Command::Start(job) if !takes => Some(job.run_id.clone()),
                    _ => None,
                });
                starts.unaccounted.extend(withheld);
                let acks = Acks::of(&reply, &check);
                let jobs = self.real_jobs.as_mut();
                let noted = jobs.map(|jobs| jobs.noted(id, &check.commands, &acks, capacity, io));
                let skipped = noted.iter().flat_map(|noted| noted.unaccounted.iter());
                starts.unaccounted.extend(skipped.cloned());
                drop(check);
                let created = hand_over(real, &mut self.book, &mut reply, starts, io);
                let jobs = self.real_jobs.as_mut().zip(noted);
                jobs.into_iter()
                    .for_each(|(jobs, noted)| jobs.landed(noted, &created, real, io));
                return;
            }
        }
        if let Some(jobs) = self.jobs.as_mut() {
            let acks = Acks::of(&reply, &check);
            jobs.answered(id, &check.commands, &acks, capacity, io);
        }
    }

    /// Writes into the ledger each start of `commands` the cell can account for, with the
    /// time of the heartbeat it names, and each stop or cancel of a run the agent runs
    /// now; before any of them reaches the agent.
    fn seen(&self, commands: &[&Command], capacity: &Resources, io: &mut Io) {
        let running = self.agent.running();
        for command in commands {
            match command {
                Command::Start(job) => {
                    if let Some(info) = RunInfo::of(job, capacity) {
                        let asked_at = sent_for(&self.beats_sent, job, io.now);
                        io.ledger.start_seen(StartSeen {
                            host: io.host.clone(),
                            run: RunId::new(job.run_id.clone()),
                            lease: info.lease,
                            at: io.now,
                            asked_at,
                            wall: info.wall,
                        });
                    }
                }
                Command::Stop { run_id } => self.stop_seen(run_id, StopCommand::Stop, &running, io),
                Command::Cancel { run_id } => {
                    self.stop_seen(run_id, StopCommand::Cancel, &running, io);
                }
                _ => {}
            }
        }
    }

    /// Writes a stop or cancel of `run` into the ledger when the agent runs it now.
    fn stop_seen(&self, run: &str, command: StopCommand, running: &BTreeSet<String>, io: &mut Io) {
        let info = self.book.runs.get(run).filter(|_| running.contains(run));
        if let Some(info) = info {
            io.ledger.stop_seen(StopSeen {
                host: io.host.clone(),
                run: RunId::new(run),
                lease: info.lease,
                at: io.now,
                command,
                wall_ends_at: info.wall_ends_at,
            });
        }
    }
}

/// Whether `command` is a start.
fn is_start(command: &&Command) -> bool {
    matches!(command, Command::Start(_))
}

/// When the heartbeat `job` names was sent: the hello that opened the session for a number
/// never sent, and `now` when there is neither.
fn sent_for(beats_sent: &BTreeMap<u64, FarmTime>, job: &JobSpec, now: FarmTime) -> FarmTime {
    beats_sent
        .get(&job.heartbeat_seq)
        .or_else(|| beats_sent.get(&0))
        .copied()
        .unwrap_or(now)
}

/// The starts of a reply the real lifecycle is about to be handed.
#[derive(Default)]
struct Starts {
    /// Each start the cell can account for, by run id, with what it runs.
    accounted: BTreeMap<String, (JobSpec, RunInfo)>,
    /// The run ids of the starts it cannot: taken out of the reply.
    unaccounted: BTreeSet<String>,
}

impl Book {
    /// Notes each command of a reply the real lifecycle is about to be handed: a stop or
    /// cancel, and a start, counted as [`Book::note_start`] says. A start the cell cannot
    /// account for is counted in [`Counter::ReplyInvalid`].
    fn starts(&mut self, commands: &[&Command], capacity: &Resources, io: &mut Io) -> Starts {
        let mut starts = Starts::default();
        for command in commands {
            match command {
                Command::Start(job) => match RunInfo::of(job, capacity) {
                    Some(info) => {
                        self.note_start(&job.run_id, io);
                        let start = ((**job).clone(), info);
                        starts.accounted.insert(job.run_id.clone(), start);
                    }
                    None => {
                        io.bump(Counter::ReplyInvalid);
                        starts.unaccounted.insert(job.run_id.clone());
                    }
                },
                Command::Stop { run_id } | Command::Cancel { run_id } => {
                    self.stopped.insert(run_id.clone());
                }
                _ => {}
            }
        }
        starts
    }
}

/// Hands `reply` to the real lifecycle, less the starts the cell cannot account for, and
/// lets it take a step. Each container it created is a launch of the start that names it;
/// each result the reply's acknowledgements ended is acknowledged with its verdict. Returns
/// the run id of each container created, jobs' included.
fn hand_over(
    real: &mut RealAgent,
    book: &mut Book,
    reply: &mut HeartbeatReply,
    starts: Starts,
    io: &mut Io,
) -> Vec<String> {
    let Starts {
        mut accounted,
        unaccounted,
    } = starts;
    reply.commands.retain(|command| match command {
        Command::Start(job) => !unaccounted.contains(&job.run_id),
        Command::JobStart(job) => !unaccounted.contains(&job.job_id),
        Command::JobStop { job_id } => !unaccounted.contains(job_id),
        _ => true,
    });
    let owed = real.owed();
    let launched = real.apply(reply);
    let kept = real.owed();
    let ended = reply
        .result_acks
        .iter()
        .filter(|ack| owed.contains(&ack.run_id) && !kept.contains(&ack.run_id));
    let verdicts = ended.filter_map(|ack| {
        let run = RunId::new(ack.run_id.clone());
        verdict(ack.ack).map(|verdict| (run, verdict))
    });
    book.acknowledged.extend(verdicts);
    let launches: Vec<(JobSpec, RunInfo)> = launched
        .iter()
        .filter_map(|run| accounted.remove(run))
        .collect();
    for (job, info) in launches {
        book.launched(&job, info, io);
    }
    launched
}

/// Writes every verdict of `acks` this version can read into the cell's ledger, whether
/// or not the agent still holds the result.
fn note_verdicts(acks: &[ResultAck], io: &mut Io) {
    let read = acks.iter().filter_map(|ack| {
        let verdict = verdict(ack.ack)?;
        Some(Acked {
            run: RunId::new(ack.run_id.clone()),
            host: io.host.clone(),
            verdict,
            at: io.now,
        })
    });
    let read: Vec<Acked> = read.collect();
    for acked in read {
        io.ledger.acked(acked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a verdict read as another, which would put the wrong acknowledgement in
    /// the agent's ground truth, or an unknown verdict that ends a result the agent must
    /// keep sending.
    #[test]
    fn every_wire_verdict_maps_to_its_own() {
        let pairs = [
            (Ack::Accepted, Some(Verdict::Accepted)),
            (Ack::Duplicate, Some(Verdict::Duplicate)),
            (Ack::Stale, Some(Verdict::Stale)),
            (Ack::UnknownLease, Some(Verdict::UnknownLease)),
            (Ack::Mismatch, Some(Verdict::Mismatch)),
            (Ack::Unknown, None),
        ];
        for (ack, wanted) in pairs {
            assert_eq!(verdict(ack), wanted);
        }
    }

    /// Catches: a lease id read from text that is not the server's `<term>-<sequence>`.
    #[test]
    fn a_lease_id_is_read_only_in_the_servers_form() {
        assert_eq!(lease_id("3-7"), Some(LeaseId::new(3, 7)));
        assert_eq!(lease_id("lease"), None);
        assert_eq!(lease_id("3-x"), None);
        assert_eq!(lease_id("x-7"), None);
    }
}
