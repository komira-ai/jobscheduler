//! One agent process: the agent behind the wire (the [`FakeAgent`], or the real lifecycle
//! of [`RealAgent`]), the session with the server, and the runs it launched. Everything
//! here is lost when the process dies.
//!
//! The session (the hellos, the heartbeats and their numbering, the answers and what ends
//! a session) is one code for both agents. What differs is behind [`Behind`]: the fake is
//! told each command and acknowledgement one by one, as [`AgentHarness`] calls; the real
//! lifecycle is handed the whole reply and builds the held runs and results of the next
//! heartbeat itself.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldRun, HeldState, Hello, JobSpec, Outcome, Phase,
    ProtocolRange, RETRY_AFTER_CAP, ResultAck, RunResult, Welcome, WireShape, refusal,
};
use js_conformance::agent::{AgentHarness, FakeAgent};
use js_types::sched::FarmTime;
use js_types::{LeaseId, Resources};

use super::real::RealAgent;
use super::{AgentKind, AgentSpec, Io, SIM_RUN_IMAGE};
use crate::bus::{CallId, Route};
use crate::counters::Counter;
use crate::ledger::Completion;
use crate::observe::{AgentTruth, Launch, RunId, Running, Verdict};

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
    /// The heartbeats not yet answered, each with whether it was already counted as
    /// missed (it had no answer when the next one was due).
    unanswered: BTreeMap<CallId, bool>,
    /// Whether a heartbeat of the session was answered 200.
    accepted: bool,
}

impl Session {
    /// Takes `id` off the unanswered heartbeats, when it is one, noting a 200.
    fn answer(&mut self, id: CallId, status: u16) -> Option<()> {
        self.unanswered
            .remove(&id)
            .map(|_| self.accepted |= status == 200)
    }
}

/// A run the process launched, as its start named it.
struct RunInfo {
    /// The lease id as the start wrote it, repeated in the run's result.
    lease_text: String,
    lease: LeaseId,
    resources: Resources,
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
fn verdict(ack: Ack) -> Option<Verdict> {
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
            Behind::Fake(agent) => fake_held(agent),
            Behind::Real(real) => real.held(),
        }
    }

    /// The results not yet acknowledged, of the runs in `runs`.
    fn results(&self, runs: &BTreeMap<String, RunInfo>) -> Vec<RunResult> {
        match self {
            Behind::Fake(agent) => fake_results(agent, runs),
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
    fn launched(&mut self, job: &JobSpec, info: RunInfo, io: &mut Io) {
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
        }
    }

    /// The process's tick: a hello while not welcomed, else a heartbeat. Returns when the
    /// next tick is due.
    pub(super) fn tick(&mut self, spec: &AgentSpec, io: &mut Io) -> Duration {
        let Some(mut session) = self.session.take() else {
            self.hello(spec, io);
            return spec.hello_interval;
        };
        self.miss(&mut session);
        self.beat(&mut session, spec, io);
        let interval = session.interval;
        self.session = Some(session);
        interval
    }

    /// A timer of the process fired: the end of a run, when the tag is one. The run
    /// completes unless it no longer runs (it was stopped, or this is the end of a second
    /// launch of a run that already ended).
    pub(super) fn timer(&mut self, tag: u64, io: &mut Io) {
        let Some(run) = self.book.run_timers.remove(&tag) else {
            return;
        };
        let ended = match &mut self.agent {
            Behind::Fake(agent) => fake_run_ends(agent, &run),
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
        }
    }

    fn hello(&mut self, spec: &AgentSpec, io: &mut Io) {
        let hello = Hello {
            protocol: ProtocolRange::THIS,
            host_id: spec.host.as_str().to_owned(),
            pool: spec.pool.clone(),
            agent_version: AGENT_VERSION.to_owned(),
            instance: None,
            shape: WireShape::from_resources(&spec.capacity),
            capabilities: spec.capabilities.clone(),
            gpu_devices: Vec::new(),
            labels: spec.labels.clone(),
            phase: Phase::Serving,
            held: self.agent.held(),
            accepts_host_credential: false,
        };
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&hello).unwrap_or_default();
        let id = io.call(Route::Hello, &spec.join_token, body);
        self.hellos.insert(id, io.now);
    }

    /// Tells the agent its connection dropped once for each heartbeat that is still not
    /// answered now that the next one is due. The real lifecycle has no such call: it
    /// learns nothing from a lost answer.
    fn miss(&mut self, session: &mut Session) {
        let missed = session.unanswered.values_mut().filter(|missed| !**missed);
        for missed in missed {
            *missed = true;
            if let Some(agent) = self.agent.fake() {
                agent.disconnect();
            }
            self.disconnected = true;
        }
    }

    /// Sends the next heartbeat. The real lifecycle takes a step first, as the real agent
    /// does on each tick, so the body carries what it found.
    fn beat(&mut self, session: &mut Session, spec: &AgentSpec, io: &mut Io) {
        if let Behind::Real(real) = &mut self.agent {
            real.step();
        }
        session.seq += 1;
        self.beats_sent.insert(session.seq, io.now);
        let results = self.agent.results(&self.book.runs);
        self.book.count_resent(&results, io);
        let beat = Heartbeat {
            host_id: spec.host.as_str().to_owned(),
            epoch: session.epoch,
            session: session.secret.clone(),
            phase: Phase::Serving,
            held: self.agent.held(),
            results,
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
            log_grant_state: None,
        };
        if let Some(agent) = self.agent.fake() {
            agent.heartbeat();
        }
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&beat).unwrap_or_default();
        let id = io.call(Route::Heartbeat, &spec.join_token, body);
        session.unanswered.insert(id, false);
    }

    fn reconnect(&mut self) {
        if self.disconnected {
            self.disconnected = false;
            if let Some(agent) = self.agent.fake() {
                agent.reconnect();
            }
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
            unanswered: BTreeMap::new(),
            accepted: false,
        });
        io.arm_tick(interval);
    }

    fn beat_answered(&mut self, answer: Answer, spec: &AgentSpec, io: &mut Io) {
        match answer.status {
            200 => self.act(&answer.body, &spec.capacity, io),
            409 => self.session_lost(spec, io),
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

    /// Acts on a heartbeat reply: every command and acknowledgement that passes the
    /// wire's checks. The fake is told each; the real lifecycle is handed the reply, with
    /// the starts the cell cannot account for taken out.
    fn act(&mut self, body: &str, capacity: &Resources, io: &mut Io) {
        let Ok(mut reply) = serde_json::from_str::<HeartbeatReply>(body) else {
            return io.bump(Counter::ReplyInvalid);
        };
        let check = reply.validate();
        if !check.refused.is_empty() || check.acks.is_err() {
            io.bump(Counter::ReplyInvalid);
        }
        match &mut self.agent {
            Behind::Fake(agent) => {
                agent.contact();
                for command in &check.commands {
                    let book = &mut self.book;
                    fake_command(agent, book, &self.beats_sent, command, capacity, io);
                }
                if check.acks.is_ok() {
                    for ack in &reply.result_acks {
                        fake_ack(agent, &mut self.book, ack);
                    }
                }
            }
            Behind::Real(real) => {
                let starts = self.book.starts(&check.commands, capacity, io);
                drop(check);
                hand_over(real, &mut self.book, &mut reply, starts, io);
            }
        }
    }
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
/// each result the reply's acknowledgements ended is acknowledged with its verdict.
fn hand_over(
    real: &mut RealAgent,
    book: &mut Book,
    reply: &mut HeartbeatReply,
    starts: Starts,
    io: &mut Io,
) {
    let Starts {
        mut accounted,
        unaccounted,
    } = starts;
    reply.commands.retain(
        |command| !matches!(command, Command::Start(job) if unaccounted.contains(&job.run_id)),
    );
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
}

/// The runs the fake holds: those it runs, and those whose result is not yet
/// acknowledged.
fn fake_held(agent: &FakeAgent) -> Vec<HeldRun> {
    let observed = agent.observed();
    let unacked = agent.unacked();
    let mut held: BTreeMap<&str, HeldState> = unacked
        .iter()
        .map(|run| (run.as_str(), HeldState::ExitedUnacked))
        .collect();
    held.extend(
        observed
            .running
            .iter()
            .map(|run| (run.as_str(), HeldState::Running)),
    );
    let held = held.into_iter().map(|(run, state)| HeldRun {
        run_id: run.to_owned(),
        state,
    });
    held.collect()
}

/// The results the fake has not had acknowledged, of the runs in `runs`.
fn fake_results(agent: &FakeAgent, runs: &BTreeMap<String, RunInfo>) -> Vec<RunResult> {
    let unacked = agent.unacked();
    let results = runs
        .iter()
        .filter(|(run, _)| unacked.contains(*run))
        .map(|(run, info)| RunResult {
            run_id: run.clone(),
            lease_id: info.lease_text.clone(),
            outcome: Outcome::Succeeded,
            exit_code: Some(0),
            detail: None,
            log_tail: None,
        });
    results.collect()
}

/// Tells the fake one command of a reply; `beats_sent` dates the heartbeat a start names.
fn fake_command(
    agent: &mut FakeAgent,
    book: &mut Book,
    beats_sent: &BTreeMap<u64, FarmTime>,
    command: &Command,
    capacity: &Resources,
    io: &mut Io,
) {
    match command {
        Command::Start(job) => fake_start(agent, book, beats_sent, job, capacity, io),
        Command::Stop { run_id } | Command::Cancel { run_id } => {
            book.stopped.insert(run_id.clone());
            agent.cancel(run_id);
        }
        _ => {}
    }
}

fn fake_start(
    agent: &mut FakeAgent,
    book: &mut Book,
    beats_sent: &BTreeMap<u64, FarmTime>,
    job: &JobSpec,
    capacity: &Resources,
    io: &mut Io,
) {
    let Some(info) = RunInfo::of(job, capacity) else {
        return io.bump(Counter::ReplyInvalid);
    };
    let run = job.run_id.as_str();
    let before = agent.observed();
    if book.stopped.contains(run) {
        io.bump(Counter::StartAfterStop);
    } else if before.launches.contains_key(run) {
        io.bump(Counter::StartRepeated);
    }
    let since = beats_sent
        .get(&job.heartbeat_seq)
        .or_else(|| beats_sent.get(&0))
        .copied()
        .unwrap_or(io.now);
    let waited = io.now.as_millis().saturating_sub(since.as_millis());
    agent.start(run, job.valid_for_ms, waited);
    let after = agent.observed();
    if after.never_started.len() > before.never_started.len() {
        io.bump(Counter::StartDroppedLate);
    }
    if after.launches.get(run) != before.launches.get(run) {
        book.launched(job, info, io);
    }
}

fn fake_ack(agent: &mut FakeAgent, book: &mut Book, ack: &ResultAck) {
    let Some(verdict) = verdict(ack.ack) else {
        return;
    };
    if agent.unacked().contains(&ack.run_id) {
        let run = RunId::new(ack.run_id.clone());
        book.acknowledged.insert(run, verdict);
    }
    agent.ack(&ack.run_id);
}

/// The run's own time ran out on the fake: it completes, and `true`, unless it no longer
/// runs.
fn fake_run_ends(agent: &mut FakeAgent, run: &str) -> bool {
    let running = agent.observed().running.contains(run);
    if running {
        agent.complete(run);
    }
    running
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
