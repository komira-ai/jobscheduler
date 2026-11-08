//! The per-job half of a real agent's process, driven call by call with no server: the
//! lifecycle behind it launches, lists, retires and ends jobs, and the ledger and counters
//! read what the fake's half writes.

use js_agent_wire::{
    Ack, BookedResources, HeartbeatReply, JobResultAck, JobStartSpec, Outcome, PER_JOB_CAPABILITY,
};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::sched::FarmTime;

use super::super::SIM_RUN_IMAGE;
use super::*;
use crate::agent::Prepare;
use crate::counters::Counters;
use crate::ledger::Ledger;

const JOB: &str = "sim-job-0";

fn allocation() -> String {
    "0a".repeat(16)
}

fn job(id: &str) -> JobId {
    JobId::parse(id).expect("a job id")
}

fn capacity() -> Resources {
    Resources::new(8_000, 16 << 30, 1, Some("gpu-a".to_owned())).expect("resources")
}

fn host() -> HostId {
    HostId::new("h000").expect("a host id")
}

fn spec(kind: AgentKind, per_job: Option<&str>) -> AgentSpec {
    let capabilities = per_job.map(|value| (PER_JOB_CAPABILITY.to_owned(), value.to_owned()));
    AgentSpec {
        host: host(),
        pool: "burst".to_owned(),
        join_token: "join".to_owned(),
        capacity: capacity(),
        capabilities: capabilities.into_iter().collect(),
        labels: BTreeMap::new(),
        hello_interval: std::time::Duration::from_secs(1),
        kind,
        instance: None,
        worker_lag: std::time::Duration::ZERO,
    }
}

/// What a heartbeat lists in `jobs`, as `(job, state, outcome)`.
type Listed = Vec<(String, HeldState, Option<Outcome>)>;

/// A real agent's per-job half and its agent, its roots under a directory of the test's own.
struct Rig {
    jobs: RealJobs,
    real: RealAgent,
    io: Io,
    _dir: TestDir,
}

fn rig(name: &str) -> Rig {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let kind = AgentKind::Real(dir.path().to_path_buf());
    let jobs = RealJobs::of(&spec(kind, Some("true"))).expect("speaks per-job");
    let real = RealAgent::new(dir.path(), &host(), &capacity()).expect("a real agent");
    let io = Io {
        now: FarmTime::from_millis(1_000),
        host: host(),
        out: Vec::new(),
        next_call: 0,
        next_tag: 0,
        tick: None,
        counters: Counters::default(),
        ledger: Ledger::new(),
        prepare: Prepare::default(),
    };
    Rig {
        jobs,
        real,
        io,
        _dir: dir,
    }
}

fn start_of(job: &str, allocation: &str) -> Command {
    Command::JobStart(Box::new(JobStartSpec {
        job_id: job.to_owned(),
        allocation_id: allocation.to_owned(),
        image: SIM_RUN_IMAGE.to_owned(),
        command: vec!["500".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: BTreeMap::new(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1_000,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
    }))
}

fn stop(job: &str) -> Command {
    Command::JobStop {
        job_id: job.to_owned(),
    }
}

impl Rig {
    /// A heartbeat is sent, as the process does: the lifecycle steps, the result is settled,
    /// and the listing goes to the ledger. Returns what it lists in `jobs` as
    /// `(job, state, outcome)` and the retired ids.
    fn beat(&mut self) -> (Listed, Vec<String>) {
        self.real.step();
        self.jobs.settle(&self.real, &mut self.io);
        let listing = RealJobs::listing(&self.real);
        let held = listing.held.iter().map(|held| {
            let outcome = held.result.as_ref().map(|result| result.outcome);
            (held.job_id.clone(), held.state, outcome)
        });
        let held = held.collect();
        let retired = listing.retired.clone();
        let call = CallId(self.io.next_call);
        self.io.next_call += 1;
        self.jobs.sent(call, listing, &mut self.io);
        (held, retired)
    }

    /// An answer to the last heartbeat with `commands` and the verdicts `acks`, handed to the
    /// lifecycle as the process hands a reply.
    fn answer(&mut self, commands: Vec<Command>, acks: &[(&str, Ack)]) {
        let job_acks = acks.iter().map(|(job, ack)| JobResultAck {
            job_id: (*job).to_owned(),
            ack: *ack,
        });
        let reply = HeartbeatReply {
            commands,
            job_acks: job_acks.collect(),
            ..HeartbeatReply::default()
        };
        let check = reply.validate();
        let handed: Vec<&Command> = reply.commands.iter().collect();
        let acks = Acks::of(&reply, &check);
        let call = CallId(self.io.next_call.saturating_sub(1));
        let noted = self
            .jobs
            .noted(call, &handed, &acks, &capacity(), &mut self.io);
        let mut reply = reply.clone();
        let skipped = &noted.unaccounted;
        reply.commands.retain(|command| match command {
            Command::JobStart(spec) => !skipped.contains(&spec.job_id),
            Command::JobStop { job_id } => !skipped.contains(job_id),
            _ => true,
        });
        let created = self.real.apply(&reply);
        self.jobs.landed(noted, &created, &self.real, &mut self.io);
    }

    /// Fires the end timer of the one job that has one.
    fn run_out(&mut self) -> bool {
        let tag = *self.jobs.timers.keys().next().expect("an end timer");
        self.jobs.timer(tag, &mut self.real, &mut self.io)
    }

    fn launches(&self) -> Vec<String> {
        let launches = self.io.ledger.launches().into_iter();
        launches
            .map(|launch| launch.run.as_str().to_owned())
            .collect()
    }
}

/// Asserts that a start launches the job once, named `<job>/<allocation>` in the ledger,
/// that the job is listed running and in the ground truth with its booked resources; that
/// its end timer makes a `succeeded` result, listed with it and written once into the
/// ledger however many heartbeats carry it, and counted as sent again; and that a verdict
/// drops it. Catches a node that launches nothing the lifecycle started, lists nothing of
/// the lifecycle's jobs, or writes a result twice or never.
#[test]
fn a_started_job_runs_ends_and_is_acknowledged() {
    let mut rig = rig("real-jobs-runs");
    rig.answer(vec![start_of(JOB, &allocation())], &[]);
    assert_eq!(rig.launches(), [format!("{JOB}/{}", allocation())]);
    let truth = rig.jobs.truth(&rig.real);
    let booked = Resources::new(1_000, 1 << 20, 0, None).expect("resources");
    assert_eq!(truth[&job(JOB)].resources, booked);
    assert!(rig.jobs.runs_unlisted(&rig.real));
    let (held, _) = rig.beat();
    assert_eq!(held, [(JOB.to_owned(), HeldState::Running, None)]);
    assert!(!rig.jobs.runs_unlisted(&rig.real));

    assert!(rig.run_out());
    assert!(rig.jobs.truth(&rig.real).is_empty());
    let (held, _) = rig.beat();
    let done = (
        JOB.to_owned(),
        HeldState::ExitedUnacked,
        Some(Outcome::Succeeded),
    );
    assert_eq!(held, [done]);
    rig.beat();
    assert_eq!(rig.io.counters.get(Counter::ResultResent), 1);
    assert_eq!(rig.io.ledger.jobs_after([0; 3]).2.len(), 1);

    rig.answer(Vec::new(), &[(JOB, Ack::Accepted)]);
    let replies = rig.io.ledger.jobs_after([0; 3]).1;
    assert_eq!(replies.last().expect("an answer").acks.len(), 1);
    assert!(rig.beat().0.is_empty());
}

/// Asserts that a start for a job the agent holds launches nothing and is counted as a
/// repeat, and that the start of a job a stop retired runs nothing and is counted as a start
/// after a stop, while the retired id is listed until an answer carries it; that a stop
/// of a running job ends it `killed` and writes its result; and that only the first stop
/// of a job no answer handed a start of, before it or in the same answer, counts as a stop
/// before the start. Catches a node that launches a repeated or stale start, or counts neither, and
/// one that miscounts a stop before the start.
#[test]
fn a_repeated_start_a_retired_job_and_a_stop_are_counted_and_listed() {
    let mut rig = rig("real-jobs-stops");
    rig.answer(vec![start_of(JOB, &allocation())], &[]);
    rig.answer(vec![start_of(JOB, &allocation())], &[]);
    assert_eq!(rig.launches().len(), 1);
    assert_eq!(rig.io.counters.get(Counter::StartRepeated), 1);

    rig.answer(vec![stop(JOB)], &[]);
    let (held, _) = rig.beat();
    let killed = (
        JOB.to_owned(),
        HeldState::ExitedUnacked,
        Some(Outcome::Killed),
    );
    assert_eq!(held, [killed]);
    assert_eq!(rig.io.ledger.jobs_after([0; 3]).2.len(), 1);

    assert_eq!(rig.io.counters.get(Counter::StopBeforeStart), 0);

    let other = "sim-job-1";
    rig.answer(vec![stop(other)], &[]);
    rig.answer(vec![stop(other)], &[]);
    assert_eq!(rig.io.counters.get(Counter::StopBeforeStart), 1);
    let (_, retired) = rig.beat();
    assert_eq!(retired, [other]);
    rig.answer(vec![start_of(other, &allocation())], &[]);
    assert_eq!(rig.io.counters.get(Counter::StartAfterStop), 1);
    assert_eq!(rig.launches().len(), 1);

    let third = "sim-job-2";
    rig.answer(vec![start_of(third, &allocation()), stop(third)], &[]);
    assert_eq!(rig.io.counters.get(Counter::StopBeforeStart), 1);
}

/// Asserts that a start or stop that names no valid id is counted and never reaches the
/// lifecycle: nothing is launched, the answer reads no start and no stop, and a timer that
/// ends no job is not the per-job half's. Catches a node that launches a job it cannot name.
#[test]
fn a_start_or_stop_that_names_no_valid_id_is_refused() {
    let mut rig = rig("real-jobs-invalid");
    let commands = vec![
        start_of("a/b", &allocation()),
        start_of(JOB, "xyz"),
        stop("a b"),
    ];
    rig.answer(commands, &[]);
    assert_eq!(rig.io.counters.get(Counter::ReplyInvalid), 3);
    assert!(rig.launches().is_empty());
    assert!(rig.real.held_jobs().is_empty());
    let replies = rig.io.ledger.jobs_after([0; 3]).1;
    assert!(replies[0].starts.is_empty() && replies[0].stops.is_empty());
    assert!(!rig.jobs.timer(7, &mut rig.real, &mut rig.io));
}

/// Asserts that a command that names no job (a cancel of a run) is left alone by the
/// per-job half: nothing is refused, launched or taken out of the reply.
#[test]
fn a_command_that_names_no_job_is_left_alone() {
    let mut rig = rig("real-jobs-run-command");
    let commands = vec![Command::Cancel {
        run_id: "run-1".to_owned(),
    }];
    let reply = HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    };
    let check = reply.validate();
    let handed: Vec<&Command> = reply.commands.iter().collect();
    let acks = Acks::of(&reply, &check);
    let noted = rig
        .jobs
        .noted(CallId(0), &handed, &acks, &capacity(), &mut rig.io);
    assert!(noted.unaccounted.is_empty());
    assert_eq!(rig.io.counters.get(Counter::ReplyInvalid), 0);
    assert!(rig.launches().is_empty());
}

/// Asserts that only a real agent whose hello reports the capability as `true` has the
/// per-job half. Catches a real node that speaks per-job messages to a server that never
/// asked, and a fake's half that shadows it.
#[test]
fn only_a_real_agent_that_reports_the_capability_speaks_per_job() {
    let real = || AgentKind::Real(std::path::PathBuf::from("unused"));
    assert!(RealJobs::of(&spec(real(), Some("true"))).is_some());
    assert!(RealJobs::of(&spec(real(), None)).is_none());
    assert!(RealJobs::of(&spec(real(), Some("false"))).is_none());
    assert!(RealJobs::of(&spec(AgentKind::Fake(None), Some("true"))).is_none());
}
