//! The per-job half of a fake agent's process, driven call by call with no server: what it
//! launches, lists, retires and acknowledges, and what it writes into the ledger.

use js_agent_wire::{Ack, BookedResources, JobResultAck};
use js_conformance::agent::Defect;
use js_types::HostId;

use super::*;
use crate::agent::Prepare;
use crate::counters::Counters;
use crate::ledger::Ledger;
use crate::observe::Verdict;

const JOB: &str = "sim-job-0";
const OTHER: &str = "sim-job-1";

/// The allocation every start here names.
fn allocation() -> String {
    "0a".repeat(16)
}

fn job(id: &str) -> JobId {
    JobId::parse(id).expect("a job id")
}

/// Every host here: 8 cores, 16 GiB, one GPU.
fn capacity() -> Resources {
    Resources::new(8_000, 16 << 30, 1, Some("gpu-a".to_owned())).expect("resources")
}

/// A fake agent's spec of `kind`, reporting `per_job` as its per-job capability when given.
fn spec(kind: AgentKind, per_job: Option<&str>) -> AgentSpec {
    let capabilities = per_job.map(|value| (PER_JOB_CAPABILITY.to_owned(), value.to_owned()));
    AgentSpec {
        host: HostId::new("h000").expect("a host id"),
        pool: "burst".to_owned(),
        join_token: "join".to_owned(),
        capacity: capacity(),
        capabilities: capabilities.into_iter().collect(),
        labels: BTreeMap::new(),
        hello_interval: Duration::from_secs(1),
        kind,
        instance: None,
        worker_lag: std::time::Duration::ZERO,
    }
}

/// The per-job half of a fake agent lacking `defect`, if one is named.
fn agent(defect: Option<Defect>) -> PerJob {
    PerJob::of(&spec(AgentKind::Fake(defect), Some("true"))).expect("speaks per-job")
}

fn fresh_io() -> Io {
    Io {
        now: FarmTime::from_millis(1_000),
        host: HostId::new("h000").expect("a host id"),
        out: Vec::new(),
        next_call: 0,
        next_tag: 0,
        tick: None,
        counters: Counters::default(),
        ledger: Ledger::new(),
        prepare: Prepare::default(),
        profile: crate::AgentProfile::default(),
    }
}

/// A start of `job` under `allocation`, its command `command`.
fn start_of(job: &str, allocation: &str, command: &[&str]) -> Command {
    Command::JobStart(Box::new(JobStartSpec {
        job_id: job.to_owned(),
        allocation_id: allocation.to_owned(),
        image: SIM_RUN_IMAGE.to_owned(),
        command: command.iter().map(|part| (*part).to_owned()).collect(),
        run: None,
        os: None,
        arch: None,
        env: BTreeMap::new(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1_000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
    }))
}

/// A start of `job` that runs 500 ms.
fn start(job: &str) -> Command {
    start_of(job, &allocation(), &["500"])
}

fn stop(job: &str) -> Command {
    Command::JobStop {
        job_id: job.to_owned(),
    }
}

/// An answer to the agent's last heartbeat with `commands`, the verdicts `acks` and the
/// retired ids `retired`.
fn answer(
    agent: &mut PerJob,
    io: &mut Io,
    commands: Vec<Command>,
    acks: &[(&str, Ack)],
    retired: &[&str],
) {
    let job_acks = acks.iter().map(|(job, ack)| JobResultAck {
        job_id: (*job).to_owned(),
        ack: *ack,
    });
    let reply = HeartbeatReply {
        commands,
        job_acks: job_acks.collect(),
        retired_acks: retired.iter().map(|id| (*id).to_owned()).collect(),
        ..HeartbeatReply::default()
    };
    let commands: Vec<&Command> = reply.commands.iter().collect();
    let call = CallId(io.next_call.saturating_sub(1));
    let check = reply.validate();
    agent.answered(call, &commands, &Acks::of(&reply, &check), &capacity(), io);
}

/// What a heartbeat lists in `jobs`, as `(job, state, outcome)`.
type Listed = Vec<(String, HeldState, Option<Outcome>)>;

/// A heartbeat: what it lists as `(job, state, outcome)` and the retired ids it lists.
fn beat(agent: &mut PerJob, io: &mut Io) -> (Listed, Vec<String>) {
    let listing = agent.listing();
    let held = listing.held.iter().map(|held| {
        let outcome = held.result.as_ref().map(|result| result.outcome);
        (held.job_id.clone(), held.state, outcome)
    });
    let held = held.collect();
    let retired = listing.retired.clone();
    let call = CallId(io.next_call);
    io.next_call += 1;
    agent.sent(call, listing, io);
    (held, retired)
}

/// Fires the end timer of the one job that has one.
fn run_out(agent: &mut PerJob, io: &mut Io) -> bool {
    let tag = *agent.timers.keys().next().expect("an end timer");
    agent.timer(tag, io)
}

fn launches(io: &Io) -> Vec<String> {
    let launches = io.ledger.launches().into_iter();
    launches
        .map(|launch| launch.run.as_str().to_owned())
        .collect()
}

/// Asserts that a start for a job the agent already holds launches nothing: one launch in
/// the ledger, named `<job>/<allocation>`, the repeat counted, and the job in the agent's
/// truth under its allocation with its booked resources; and that an agent lacking
/// `StartRunsTwice`'s fix launches it again. Catches an agent that relaunches a repeated
/// start, and one that keys its jobs by something other than the job id.
#[test]
fn a_repeated_jobstart_for_a_held_job_runs_once() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(&mut agent, &mut io, vec![start(JOB)], &[], &[]);
    answer(&mut agent, &mut io, vec![start(JOB)], &[], &[]);
    let named = format!("{JOB}/{}", allocation());
    assert_eq!(launches(&io), [named.as_str()]);
    assert_eq!(io.counters.get(Counter::StartRepeated), 1);
    let truth = agent.truth();
    let running = truth.get(&job(JOB)).expect("the job runs");
    assert_eq!(running.allocation.to_string(), allocation());
    let booked = Resources::new(1_000, 1 << 30, 0, None).expect("resources");
    assert_eq!(running.resources, booked);
    let replies = io.ledger.jobs_after([0; 3]).1;
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0].starts[0].job, job(JOB));

    let mut broken = PerJob::of(&spec(
        AgentKind::Fake(Some(Defect::StartRunsTwice)),
        Some("true"),
    ))
    .expect("speaks per-job");
    let mut io = fresh_io();
    answer(&mut broken, &mut io, vec![start(JOB)], &[], &[]);
    answer(&mut broken, &mut io, vec![start(JOB)], &[], &[]);
    assert_eq!(launches(&io), [named.as_str(), named.as_str()]);
}

/// Asserts that a job the agent runs is unlisted until a heartbeat lists it, and stays
/// listed once one has, running or with its result; and that an agent that runs no job,
/// or holds only a result, runs nothing unlisted. Catches a cut agent's job counted as seen
/// by the server before any heartbeat carried it (the silent-host witness would then never
/// hold for jobs), and one counted unlisted after a heartbeat listed it.
#[test]
fn a_job_is_unlisted_until_a_heartbeat_lists_it() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    assert!(!agent.runs_unlisted());
    answer(&mut agent, &mut io, vec![start(JOB)], &[], &[]);
    assert!(agent.runs_unlisted());
    beat(&mut agent, &mut io);
    assert!(!agent.runs_unlisted());
    answer(&mut agent, &mut io, vec![start(OTHER)], &[], &[]);
    assert!(agent.runs_unlisted());
    beat(&mut agent, &mut io);
    assert!(!agent.runs_unlisted());
    assert!(run_out_job(&mut agent, &mut io, OTHER));
    assert!(!agent.runs_unlisted());
}

/// Fires the end timer of `job`.
fn run_out_job(agent: &mut PerJob, io: &mut Io, job: &str) -> bool {
    let tag = agent.timers.iter().find(|(_, held)| held.as_str() == job);
    let tag = *tag.expect("an end timer of the job").0;
    agent.timer(tag, io)
}

/// Asserts that a stop of a job the agent never held retires its id: listed in
/// `retired_jobs`, not in `jobs`, and counted once as a stop before the start (a second
/// stop of it is not counted again), and a later start of it is refused, counted as a start
/// after a stop, and written into the ledger as refused (a start it took is not). Catches
/// an agent that ignores a stop of a job it does not hold (the server would never free the
/// allocation), one that runs a start after the stop, a refused start taken by the
/// invariants for one the agent acted on, and a stop before the start that goes uncounted
/// or is counted for each repeat.
#[test]
fn a_jobstop_for_an_unknown_job_retires_its_id() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(&mut agent, &mut io, vec![stop(JOB)], &[], &[]);
    assert_eq!(io.counters.get(Counter::StopBeforeStart), 1);
    let (held, retired) = beat(&mut agent, &mut io);
    assert!(held.is_empty());
    assert_eq!(retired, [JOB]);
    answer(&mut agent, &mut io, vec![start(JOB)], &[], &[]);
    assert!(launches(&io).is_empty());
    assert_eq!(io.counters.get(Counter::StartAfterStop), 1);
    let (beats, replies, _) = io.ledger.jobs_after([0; 3]);
    assert_eq!(beats[0].retired, [job(JOB)].into());
    assert!(replies[1].starts[0].refused);
    answer(&mut agent, &mut io, vec![start(OTHER)], &[], &[]);
    let replies = io.ledger.jobs_after([0; 3]).1;
    assert!(!replies[2].starts[0].refused);
    answer(&mut agent, &mut io, vec![stop(JOB)], &[], &[]);
    assert_eq!(io.counters.get(Counter::StopBeforeStart), 1);
}

/// Asserts that a result is listed with every heartbeat, `exited_unacked` and `succeeded`,
/// until an answer acknowledges it with a verdict this version reads (an `Unknown` one
/// keeps it), and is then dropped; that the ledger has the result made once and the
/// verdict read; and that an agent lacking `ForgetsUnackedResult`'s fix drops it when its
/// connection drops. Catches an agent that sends a result once, and one that drops it on
/// a verdict it cannot read.
#[test]
fn a_result_is_resent_until_acked() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(&mut agent, &mut io, vec![start(JOB)], &[], &[]);
    assert_eq!(
        beat(&mut agent, &mut io).0,
        [(JOB.to_owned(), HeldState::Running, None)]
    );
    assert!(run_out(&mut agent, &mut io));
    let unacked = [(
        JOB.to_owned(),
        HeldState::ExitedUnacked,
        Some(Outcome::Succeeded),
    )];
    assert_eq!(beat(&mut agent, &mut io).0, unacked);
    assert_eq!(beat(&mut agent, &mut io).0, unacked);
    answer(&mut agent, &mut io, Vec::new(), &[(JOB, Ack::Unknown)], &[]);
    assert_eq!(beat(&mut agent, &mut io).0, unacked);
    answer(
        &mut agent,
        &mut io,
        Vec::new(),
        &[(JOB, Ack::Accepted)],
        &[],
    );
    assert!(beat(&mut agent, &mut io).0.is_empty());
    let (beats, replies, made) = io.ledger.jobs_after([0; 3]);
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].job, job(JOB));
    assert_eq!(beats[1].results, [job(JOB)].into());
    let acks: Vec<_> = replies
        .iter()
        .flat_map(|reply| reply.acks.clone())
        .collect();
    assert_eq!(acks, [(job(JOB), Verdict::Accepted)]);
    assert_eq!(io.counters.get(Counter::ResultResent), 2);

    let mut broken = PerJob::of(&spec(
        AgentKind::Fake(Some(Defect::ForgetsUnackedResult)),
        Some("true"),
    ))
    .expect("speaks per-job");
    let mut io = fresh_io();
    answer(&mut broken, &mut io, vec![start(JOB)], &[], &[]);
    assert!(run_out(&mut broken, &mut io));
    broken.disconnect();
    broken.reconnect();
    assert!(beat(&mut broken, &mut io).0.is_empty());
}

/// Asserts that a retired id is listed in every heartbeat until an answer names it in
/// `retired_acks`, and only that answer: one naming another id keeps it. Catches an agent
/// that lists a retired id once, and one that forgets every retired id on any answer.
#[test]
fn a_retired_id_is_listed_until_acked() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(&mut agent, &mut io, vec![stop(JOB), stop(OTHER)], &[], &[]);
    assert_eq!(beat(&mut agent, &mut io).1, [JOB, OTHER]);
    assert_eq!(beat(&mut agent, &mut io).1, [JOB, OTHER]);
    answer(&mut agent, &mut io, Vec::new(), &[], &[OTHER]);
    assert_eq!(beat(&mut agent, &mut io).1, [JOB]);
    answer(&mut agent, &mut io, Vec::new(), &[], &[JOB]);
    assert!(beat(&mut agent, &mut io).1.is_empty());
}

/// Asserts that a stop of a job the agent runs ends it with a `killed` result and no exit
/// code, made once in the ledger, and that a stop of that job again, while its result is
/// not yet acknowledged, keeps the result rather than retiring the id; that a job whose
/// command is not a duration runs until stopped; and that the end timer of a job a stop
/// ended makes no second result; and that no such stop counts as a stop before the start.
/// Catches an agent that drops a stopped job silently (the server would wait for its
/// result), one that reports it succeeded, and one that counts a stop of a job it was
/// handed a start of as a cancel that beat the start.
#[test]
fn a_stop_of_a_running_job_ends_it_killed_and_its_result_rides_on() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(
        &mut agent,
        &mut io,
        vec![start_of(JOB, &allocation(), &["sleep"])],
        &[],
        &[],
    );
    assert!(
        agent.timers.is_empty(),
        "a job that is not a duration has no end"
    );
    answer(&mut agent, &mut io, vec![stop(JOB)], &[], &[]);
    let listing = agent.listing();
    let result = listing.held[0].result.clone().expect("a result");
    assert_eq!((result.outcome, result.exit_code), (Outcome::Killed, None));
    assert_eq!(result.allocation_id, allocation());
    answer(&mut agent, &mut io, vec![stop(JOB)], &[], &[]);
    let (held, retired) = beat(&mut agent, &mut io);
    assert_eq!(
        held,
        [(
            JOB.to_owned(),
            HeldState::ExitedUnacked,
            Some(Outcome::Killed)
        )]
    );
    assert!(retired.is_empty());
    assert!(agent.truth().is_empty());

    answer(&mut agent, &mut io, vec![start(OTHER)], &[], &[]);
    answer(&mut agent, &mut io, vec![stop(OTHER)], &[], &[]);
    assert!(run_out(&mut agent, &mut io), "the timer was the job's");
    let made = io.ledger.jobs_after([0; 3]).2;
    let jobs: Vec<JobId> = made.into_iter().map(|made| made.job).collect();
    assert_eq!(jobs, [job(JOB), job(OTHER)]);
    assert_eq!(io.counters.get(Counter::StopBeforeStart), 0);
}

/// Asserts that a start naming no valid job id or no valid allocation id, and a stop
/// naming no valid job id, are refused and counted as invalid; that a verdict on no valid
/// job id, and a run's stop, change nothing; and that a timer that ends no job is not the
/// per-job half's. Catches an agent that launches a job it cannot name, and one that takes
/// a run's timer for a job's.
#[test]
fn a_start_or_stop_that_names_no_valid_id_is_refused() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    let commands = vec![
        start_of("a/b", &allocation(), &["500"]),
        start_of(JOB, "xyz", &["500"]),
        stop("a b"),
        Command::Stop {
            run_id: "r".to_owned(),
        },
    ];
    answer(
        &mut agent,
        &mut io,
        commands,
        &[("a b", Ack::Accepted)],
        &[],
    );
    assert_eq!(io.counters.get(Counter::ReplyInvalid), 3);
    assert!(launches(&io).is_empty());
    let replies = io.ledger.jobs_after([0; 3]).1;
    assert!(replies[0].starts.is_empty() && replies[0].acks.is_empty());
    assert!(!agent.timer(7, &mut io));
}

/// Asserts that only a fake agent whose hello reports the per-job capability as `true`
/// speaks the per-job messages: not one without it, not one reporting another value, and
/// not a real agent's node. Catches a node that speaks them to a server that never asked,
/// and a real agent's node that a fake's per-job half would shadow.
#[test]
fn only_a_fake_that_reports_the_capability_speaks_per_job() {
    assert!(PerJob::of(&spec(AgentKind::Fake(None), Some("true"))).is_some());
    assert!(PerJob::of(&spec(AgentKind::Fake(None), None)).is_none());
    assert!(PerJob::of(&spec(AgentKind::Fake(None), Some("false"))).is_none());
    let real = AgentKind::Real(std::path::PathBuf::from("unused"));
    assert!(PerJob::of(&spec(real, Some("true"))).is_none());
}

/// Asserts that an answer is written into the ledger with the heartbeat it answers: when
/// that heartbeat was sent and the jobs it listed without a result. Catches an answer
/// matched to another heartbeat, which would date a lost job's listing wrongly.
#[test]
fn an_answer_is_ledgered_with_the_heartbeat_it_answers() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(&mut agent, &mut io, vec![start(JOB)], &[], &[]);
    io.now = FarmTime::from_millis(2_000);
    beat(&mut agent, &mut io);
    io.now = FarmTime::from_millis(2_500);
    answer(&mut agent, &mut io, Vec::new(), &[], &[]);
    let replies = io.ledger.jobs_after([0; 3]).1;
    let last = replies.last().expect("an answer");
    assert_eq!(last.beat_at, FarmTime::from_millis(2_000));
    assert_eq!(last.at, FarmTime::from_millis(2_500));
    assert_eq!(last.listed, [job(JOB)].into());
}

/// Asserts that a list of verdicts or of retired ids that fails the wire's check (an id
/// named twice) is ignored whole: the result rides on and the retired id stays listed, and
/// the answer's ledger entry reads no verdict. Catches an agent that acts on a list the wire
/// says to ignore.
#[test]
fn an_ack_list_that_fails_the_wires_check_is_ignored() {
    let mut agent = agent(None);
    let mut io = fresh_io();
    answer(&mut agent, &mut io, vec![start(JOB), stop(OTHER)], &[], &[]);
    assert!(run_out(&mut agent, &mut io));
    let twice = [(JOB, Ack::Accepted), (JOB, Ack::Accepted)];
    answer(&mut agent, &mut io, Vec::new(), &twice, &[OTHER, OTHER]);
    let (held, retired) = beat(&mut agent, &mut io);
    let unacked = [(
        JOB.to_owned(),
        HeldState::ExitedUnacked,
        Some(Outcome::Succeeded),
    )];
    assert_eq!(held, unacked);
    assert_eq!(retired, [OTHER]);
    let replies = io.ledger.jobs_after([0; 3]).1;
    assert!(replies.iter().all(|reply| reply.acks.is_empty()));
}
