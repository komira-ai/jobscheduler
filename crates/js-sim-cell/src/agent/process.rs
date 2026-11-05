//! One agent process: the [`FakeAgent`], the session with the server, and the runs it
//! launched. Everything here is lost when the process dies.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldRun, HeldState, Hello, JobSpec, Outcome,
    ProtocolRange, RETRY_AFTER_CAP, ResultAck, RunResult, Welcome, WireShape, refusal,
};
use js_conformance::agent::{AgentHarness, FakeAgent};
use js_types::sched::FarmTime;
use js_types::{LeaseId, Resources};

use super::exclusive::Exclusive;
use super::{AgentSpec, Io, SIM_RUN_IMAGE};
use crate::bus::{CallId, Route};
use crate::counters::Counter;
use crate::ledger::Completion;
use crate::observe::{
    AgentTruth, Launch, ReadyReport, RunId, Running, StartSeen, StopCommand, StopSeen, Verdict,
};

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

/// One agent process.
pub(super) struct Process {
    agent: FakeAgent,
    session: Option<Session>,
    /// The hellos not yet answered, with when each was sent.
    hellos: BTreeMap<CallId, FarmTime>,
    /// When each heartbeat of the session was sent, by number; 0 is the hello the
    /// session's welcome answered.
    beats_sent: BTreeMap<u64, FarmTime>,
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
    /// Whether the agent was told its connection dropped and not yet that it is back.
    disconnected: bool,
    /// Where the process is in the exclusive handshake.
    exclusive: Exclusive,
}

impl Process {
    /// A fresh process: the agent `spec` names, holding nothing, not welcomed.
    pub(super) fn new(spec: &AgentSpec) -> Self {
        Self {
            agent: spec
                .defect
                .map_or_else(|| FakeAgent::with_window(None), FakeAgent::broken),
            session: None,
            hellos: BTreeMap::new(),
            beats_sent: BTreeMap::new(),
            runs: BTreeMap::new(),
            run_timers: BTreeMap::new(),
            stopped: BTreeSet::new(),
            reported: BTreeSet::new(),
            completed: BTreeMap::new(),
            acknowledged: BTreeMap::new(),
            disconnected: false,
            exclusive: Exclusive::default(),
        }
    }

    /// The process's ground truth, on a host of `capacity`.
    pub(super) fn truth(&self, capacity: &Resources) -> AgentTruth {
        let running = self.agent.observed().running.into_iter().filter_map(|run| {
            self.runs.get(&run).map(|info| {
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
            completed: self.completed.clone(),
            acknowledged: self.acknowledged.clone(),
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

    /// A timer of the process fired: the end of a run, when the tag is one.
    pub(super) fn timer(&mut self, tag: u64, io: &mut Io) {
        if let Some(run) = self.run_timers.remove(&tag) {
            self.run_ends(run, io);
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
            phase: self.exclusive.phase(),
            held: self.held(),
            accepts_host_credential: false,
        };
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&hello).unwrap_or_default();
        let id = io.call(Route::Hello, &spec.join_token, body);
        self.hellos.insert(id, io.now);
    }

    /// Tells the agent its connection dropped once for each heartbeat that is still not
    /// answered now that the next one is due.
    fn miss(&mut self, session: &mut Session) {
        let missed = session.unanswered.values_mut().filter(|missed| !**missed);
        for missed in missed {
            *missed = true;
            self.agent.disconnect();
            self.disconnected = true;
        }
    }

    fn beat(&mut self, session: &mut Session, spec: &AgentSpec, io: &mut Io) {
        session.seq += 1;
        self.beats_sent.insert(session.seq, io.now);
        let quiet = self.held().is_empty();
        let (ready, refused) = self.exclusive.report(io.prepare, quiet, io.now);
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
            held: self.held(),
            results: self.results(io),
            taint: None,
            exclusive_ready: ready,
            exclusive_refused: refused,
            load: None,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
            log_grant_state: None,
        };
        self.agent.heartbeat();
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&beat).unwrap_or_default();
        let id = io.call(Route::Heartbeat, &spec.join_token, body);
        session.unanswered.insert(id, false);
    }

    /// Every run the process holds: those it runs, and those whose result is not yet
    /// acknowledged.
    fn held(&self) -> Vec<HeldRun> {
        let observed = self.agent.observed();
        let unacked = self.agent.unacked();
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

    /// The results not yet acknowledged, counting each one sent before.
    fn results(&mut self, io: &mut Io) -> Vec<RunResult> {
        let unacked = self.agent.unacked();
        let results: Vec<RunResult> = self
            .runs
            .iter()
            .filter(|(run, _)| unacked.contains(*run))
            .map(|(run, info)| RunResult {
                run_id: run.clone(),
                lease_id: info.lease_text.clone(),
                outcome: Outcome::Succeeded,
                exit_code: Some(0),
                detail: None,
                log_tail: None,
            })
            .collect();
        for result in &results {
            if !self.reported.insert(result.run_id.clone()) {
                io.bump(Counter::ResultResent);
            }
        }
        results
    }

    fn reconnect(&mut self) {
        if self.disconnected {
            self.disconnected = false;
            self.agent.reconnect();
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
    /// wire's checks.
    fn act(&mut self, body: &str, capacity: &Resources, io: &mut Io) {
        let Ok(reply) = serde_json::from_str::<HeartbeatReply>(body) else {
            return io.bump(Counter::ReplyInvalid);
        };
        let check = reply.validate();
        if !check.refused.is_empty() || check.acks.is_err() || check.exclusive.is_err() {
            io.bump(Counter::ReplyInvalid);
        }
        self.agent.contact();
        if check.exclusive.is_ok() {
            self.exclusive.told(reply.exclusive.as_deref(), io.now);
        }
        for command in &check.commands {
            self.command(command, capacity, io);
        }
        if check.acks.is_ok() {
            for ack in &reply.result_acks {
                self.ack(ack);
            }
        }
    }

    fn command(&mut self, command: &Command, capacity: &Resources, io: &mut Io) {
        match command {
            Command::Start(job) => self.start(job, capacity, io),
            Command::Stop { run_id } => self.stop(run_id, StopCommand::Stop, io),
            Command::Cancel { run_id } => self.stop(run_id, StopCommand::Cancel, io),
            _ => {}
        }
    }

    fn start(&mut self, job: &JobSpec, capacity: &Resources, io: &mut Io) {
        let Some(info) = RunInfo::of(job, capacity) else {
            return io.bump(Counter::ReplyInvalid);
        };
        let run = job.run_id.as_str();
        let since = self
            .beats_sent
            .get(&job.heartbeat_seq)
            .or_else(|| self.beats_sent.get(&0))
            .copied()
            .unwrap_or(io.now);
        io.ledger.start_seen(StartSeen {
            host: io.host.clone(),
            run: RunId::new(run),
            lease: info.lease,
            at: io.now,
            asked_at: since,
            wall: info.wall,
        });
        if !self.exclusive.takes_starts() {
            return;
        }
        let before = self.agent.observed();
        if self.stopped.contains(run) {
            io.bump(Counter::StartAfterStop);
        } else if before.launches.contains_key(run) {
            io.bump(Counter::StartRepeated);
        }
        let waited = io.now.as_millis().saturating_sub(since.as_millis());
        self.agent.start(run, job.valid_for_ms, waited);
        let after = self.agent.observed();
        if after.never_started.len() > before.never_started.len() {
            io.bump(Counter::StartDroppedLate);
        }
        if after.launches.get(run) != before.launches.get(run) {
            self.launched(job, info, io);
        }
    }

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

    /// A stop or cancel of `run` arrived: the ledger notes it when the run is running.
    fn stop(&mut self, run: &str, command: StopCommand, io: &mut Io) {
        let running = self.agent.observed().running.contains(run);
        if let Some(info) = self.runs.get(run).filter(|_| running) {
            io.ledger.stop_seen(StopSeen {
                host: io.host.clone(),
                run: RunId::new(run),
                lease: info.lease,
                at: io.now,
                command,
                wall_ends_at: info.wall_ends_at,
            });
        }
        self.stopped.insert(run.to_owned());
        self.agent.cancel(run);
    }

    fn ack(&mut self, ack: &ResultAck) {
        let Some(verdict) = verdict(ack.ack) else {
            return;
        };
        if self.agent.unacked().contains(&ack.run_id) {
            let run = RunId::new(ack.run_id.clone());
            self.acknowledged.insert(run, verdict);
        }
        self.agent.ack(&ack.run_id);
    }

    /// The run's own time ran out: it completes, unless it no longer runs (it was
    /// stopped, or this is the end of a second launch of a run that already ended).
    fn run_ends(&mut self, run: String, io: &mut Io) {
        if !self.agent.observed().running.contains(&run) {
            return;
        }
        self.agent.complete(&run);
        let run = RunId::new(run);
        self.completed.insert(run.clone(), io.now);
        io.ledger.completed(Completion {
            run,
            host: io.host.clone(),
            at: io.now,
        });
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
