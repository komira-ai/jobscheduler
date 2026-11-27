//! One agent process: the agent behind the wire (the fake's per-job half, or the real
//! lifecycle of [`RealAgent`]), the session with the server, and the jobs it launched.
//! Everything here is lost when the process dies.
//!
//! The session (the hellos, the heartbeats and their numbering, the answers and what ends
//! a session) is one code for both agents. What differs is behind the per-job halves: the
//! fake's is told each command and acknowledgement one by one, as [`AgentHarness`] calls
//! (`per_job.rs`); the real lifecycle is handed the whole reply and builds the jobs of the
//! next heartbeat itself (`real_jobs.rs`). An agent of this release holds no run keyed by a
//! lease, so every hello and heartbeat lists no run (`held`) and sends no run result
//! (`results`), and an acknowledgement of a run's result in a reply is ignored.
//!
//! The whole-host handshake ([`Exclusive`]) is the session's, for both agents: neither
//! knows it. The phase of every hello and heartbeat, its ready report and its refusal come
//! from it, and a reply's `exclusive` is handed to it before the reply's commands.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent::{BASE, REQUEST_LIMIT};
use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, Hello, ProtocolRange, RETRY_AFTER_CAP, Welcome,
    WireShape, refusal,
};
use js_jobs::JobId;
use js_types::Resources;
use js_types::sched::FarmTime;

use super::exclusive::Exclusive;
use super::per_job::{Acks, Listing, PerJob};
use super::real::RealAgent;
use super::real_jobs::RealJobs;
use super::{AgentKind, AgentSpec, Io};
use crate::bus::{CallId, Route};
use crate::counters::Counter;
use crate::observe::{AgentTruth, ReadyReport, RunningJob, Verdict};

#[cfg(doc)]
use js_conformance::agent::AgentHarness;

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

/// The cell's verdict for the wire's, or `None` for one this version cannot read: the
/// agent keeps that result and sends it again.
pub(super) fn verdict(ack: Ack) -> Option<Verdict> {
    match ack {
        Ack::Accepted => Some(Verdict::Accepted),
        Ack::Duplicate => Some(Verdict::Duplicate),
        Ack::Stale => Some(Verdict::Stale),
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
    /// The id the process names in every hello ([`Hello::agent_process`]): the host and
    /// the count of processes the host has had, so it is the same across the hellos of one
    /// process, differs between processes, and draws no randomness.
    id: String,
    /// The real lifecycle, when the host runs the real agent.
    real: Option<RealAgent>,
    session: Option<Session>,
    /// The hellos not yet answered.
    hellos: BTreeSet<CallId>,
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
    pub(super) fn new(spec: &AgentSpec, generation: u64) -> Option<Self> {
        let real = match &spec.kind {
            AgentKind::Fake(_) => None,
            AgentKind::Real(root) => Some(RealAgent::new(root, &spec.host, &spec.capacity)?),
        };
        Some(Self {
            id: format!("sim-{}-{generation}", spec.host.as_str()),
            real,
            session: None,
            hellos: BTreeSet::new(),
            disconnected: false,
            exclusive: Exclusive::default(),
            jobs: PerJob::of(spec),
            real_jobs: RealJobs::of(spec),
            lag: spec.worker_lag,
            busy_until: FarmTime::default(),
            queued: BTreeMap::new(),
        })
    }

    /// Holds the host when the real agent is behind the wire ([`RealAgent::hold`]); a fake
    /// has no state root and is not held. Returns whether the host is held.
    pub(super) fn hold(&self) -> bool {
        self.real.as_ref().is_some_and(RealAgent::hold)
    }

    /// The time is `now`: the real lifecycle's clock reads it from here on.
    pub(super) fn at(&self, now: FarmTime) {
        if let Some(real) = &self.real {
            real.at(now);
        }
    }

    /// The process's ground truth, on a host of `capacity`: the jobs it runs.
    pub(super) fn truth(&self, capacity: &Resources) -> AgentTruth {
        AgentTruth {
            jobs: self.jobs_truth(),
            ..AgentTruth::new(capacity.clone())
        }
    }

    /// The jobs the agent runs now, whichever agent is behind the per-job half.
    fn jobs_truth(&self) -> BTreeMap<JobId, RunningJob> {
        let fake = self.jobs.as_ref().map(PerJob::truth);
        let jobs = self.real_jobs.as_ref().zip(self.real.as_ref());
        let real = jobs.map(|(jobs, real)| jobs.truth(real));
        fake.or(real).unwrap_or_default()
    }

    /// What the next heartbeat lists as jobs, whichever agent is behind the per-job half.
    fn jobs_listing(&self) -> Listing {
        let fake = self.jobs.as_ref().map(PerJob::listing);
        let real = self.real_jobs.as_ref().zip(self.real.as_ref());
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

    /// A timer of the process fired: the end of a job, when the tag is one, or a reply the
    /// worker has now taken its lag over. The job completes unless it no longer runs (it was
    /// stopped).
    pub(super) fn timer(&mut self, tag: u64, capacity: &Resources, io: &mut Io) {
        if let Some((id, body)) = self.queued.remove(&tag) {
            return self.act(id, &body, capacity, io);
        }
        if let Some(jobs) = self.jobs.as_mut() {
            jobs.timer(tag, io);
        }
        let real = self.real_jobs.as_mut().zip(self.real.as_mut());
        if let Some((jobs, real)) = real {
            jobs.timer(tag, real, io);
        }
    }

    /// An answer arrived. An answer to no open call (a hello already answered, a heartbeat
    /// of an ended session) is ignored.
    pub(super) fn answer(&mut self, answer: Answer, spec: &AgentSpec, io: &mut Io) {
        if self.hellos.remove(&answer.id) {
            self.reconnect();
            return self.hello_answered(answer, spec, io);
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
            agent_process: Some(self.id.clone()),
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
            held: Vec::new(),
            accepts_host_credential: false,
            identity: None,
        };
        let hello = io.profile.shape(hello);
        // A wire type always encodes; were it not to, the empty body would be refused.
        let body = serde_json::to_string(&hello).unwrap_or_default();
        let id = io.call(Route::Hello, &spec.join_token, body);
        self.hellos.insert(id);
    }

    /// Tells the fake agent its connection dropped: the heartbeat had no answer in
    /// [`REQUEST_LIMIT`]. The real lifecycle has no such call: it learns nothing from a
    /// lost answer.
    fn timed_out(&mut self) {
        self.jobs.iter_mut().for_each(PerJob::disconnect);
        self.disconnected = true;
    }

    /// Sends the next heartbeat. The real lifecycle takes a step first, as the real agent
    /// does on each tick, so the body carries what it found.
    fn beat(&mut self, session: &mut Session, spec: &AgentSpec, io: &mut Io) {
        if let Some(real) = &mut self.real {
            real.step();
        }
        self.settle(io);
        session.seq += 1;
        let listing = self.jobs_listing();
        let (ready, refused) = self.exclusive.report(io.prepare, io.now);
        if let Some(lease) = &ready {
            io.ledger.reported_ready(ReadyReport {
                host: io.host.clone(),
                lease: lease.clone(),
                at: io.now,
            });
        }
        let beat = Heartbeat {
            seq: Some(session.seq),
            host_id: spec.host.as_str().to_owned(),
            epoch: session.epoch,
            session: session.secret.clone(),
            phase: self.exclusive.phase(),
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            taint_cleared: None,
            tainted: None,
            exclusive_ready: ready,
            exclusive_refused: refused,
            load: None,
            disk_gib: None,
            jobs: listing.held.clone(),
            retired_jobs: listing.retired.clone(),
            log_grant_state: None,
        };
        let beat = io.profile.shape_beat(beat);
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
        let jobs = self.real_jobs.as_mut().zip(self.real.as_ref());
        jobs.into_iter()
            .for_each(|(jobs, real)| jobs.settle(real, io));
    }

    /// Whether the process runs a job that no heartbeat it sent listed, so the server
    /// cannot have seen it listed.
    pub(super) fn runs_unlisted(&self) -> bool {
        let fake = self.jobs.as_ref().is_some_and(PerJob::runs_unlisted);
        let real = self.real_jobs.as_ref().zip(self.real.as_ref());
        let real = real.is_some_and(|(jobs, real)| jobs.runs_unlisted(real));
        fake || real
    }

    fn reconnect(&mut self) {
        if self.disconnected {
            self.disconnected = false;
            self.jobs.iter_mut().for_each(PerJob::reconnect);
        }
    }

    fn hello_answered(&mut self, answer: Answer, spec: &AgentSpec, io: &mut Io) {
        match answer.status {
            200 => self.welcomed(&answer.body, io),
            429 => {
                let wait = answer.retry_after.unwrap_or(spec.hello_interval);
                io.arm_tick(wait.min(RETRY_AFTER_CAP));
            }
            _ => {}
        }
    }

    fn welcomed(&mut self, body: &str, io: &mut Io) {
        let welcome = serde_json::from_str::<Welcome>(body)
            .ok()
            .filter(|welcome| welcome.validate().is_ok());
        let Some(welcome) = welcome else {
            return io.bump(Counter::ReplyInvalid);
        };
        let interval = Duration::from_millis(welcome.heartbeat_interval_ms);
        self.hellos.clear();
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
    /// then every command and acknowledgement that passes the wire's checks. A job start
    /// and stop go to the per-job half, when the agent speaks the per-job messages: the
    /// fake's is told each, as the answer to the heartbeat sent as call `id`; the real
    /// lifecycle is handed the reply, with the starts and stops the cell cannot account for
    /// taken out. A run acknowledgement (`result_acks`) is ignored.
    fn act(&mut self, id: CallId, body: &str, capacity: &Resources, io: &mut Io) {
        let Ok(mut reply) = serde_json::from_str::<HeartbeatReply>(body) else {
            return io.bump(Counter::ReplyInvalid);
        };
        let check = reply.validate();
        if !check.refused.is_empty() || check.exclusive.is_err() {
            io.bump(Counter::ReplyInvalid);
        }
        if check.exclusive.is_ok() {
            self.exclusive.told(reply.exclusive.as_deref(), io.now);
        }
        let acks = Acks::of(&reply, &check);
        let Some(real) = self.real.as_mut() else {
            if let Some(jobs) = self.jobs.as_mut() {
                jobs.answered(id, &check.commands, &acks, capacity, io);
            }
            return;
        };
        let jobs = self.real_jobs.as_mut();
        let noted = jobs.map(|jobs| jobs.noted(id, &check.commands, &acks, capacity, io));
        let skipped = noted.iter().flat_map(|noted| noted.unaccounted.iter());
        let unaccounted: BTreeSet<String> = skipped.cloned().collect();
        drop(check);
        let created = hand_over(real, &mut reply, &unaccounted);
        let jobs = self.real_jobs.as_mut().zip(noted);
        jobs.into_iter()
            .for_each(|(jobs, noted)| jobs.landed(noted, &created, real, io));
    }
}

/// Hands `reply` to the real lifecycle, less the job starts and stops named in `unaccounted`
/// (the cell cannot account for them), and lets it take a step. Returns the job id of each
/// container it created.
fn hand_over(
    real: &mut RealAgent,
    reply: &mut HeartbeatReply,
    unaccounted: &BTreeSet<String>,
) -> Vec<String> {
    reply.commands.retain(|command| match command {
        Command::JobStart(job) => !unaccounted.contains(&job.job_id),
        Command::JobStop { job_id } => !unaccounted.contains(job_id),
        _ => true,
    });
    real.apply(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that every verdict the wire has maps to the cell's own, and that a verdict
    /// this version cannot read maps to none. It catches a verdict read as another, which
    /// would put the wrong acknowledgement in the agent's ground truth, or an unknown
    /// verdict that ends a result the agent must keep sending.
    #[test]
    fn every_wire_verdict_maps_to_its_own() {
        let pairs = [
            (Ack::Accepted, Some(Verdict::Accepted)),
            (Ack::Duplicate, Some(Verdict::Duplicate)),
            (Ack::Stale, Some(Verdict::Stale)),
            (Ack::Mismatch, Some(Verdict::Mismatch)),
            (Ack::Unknown, None),
        ];
        for (ack, wanted) in pairs {
            assert_eq!(verdict(ack), wanted);
        }
    }
}
