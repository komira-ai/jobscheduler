//! What a heartbeat lists stays within the body the server reads, however many starts a
//! server sends and however few results it acknowledges: a start is skipped once the held
//! entries would leave no room for a result, the results go out oldest first as far as they
//! fit (a job's entry is listed without its result meanwhile), and the retired job ids go in
//! what is left.

use js_agent::lifecycle::RETIRED_LISTED;
use js_agent_wire::limits::{self, HEARTBEAT_BODY_BUDGET};
use js_agent_wire::{Command, Heartbeat, HeldState, Phase};
use serde_json::json;

use super::logs::full_load;
use super::per_job::{job_start, start};
use super::{Harness, job, names, reply_json};

/// The entries a heartbeat takes at the longest ids: the body (65,536 bytes) less the
/// largest load (28,672), the envelope (4,096) and one result at its largest (8,192) leaves
/// 24,576 bytes, and an entry at a 128-byte id and its longest state weighs 167.
const LISTABLE: usize = 147;

/// An id of 128 bytes, the longest, ending in `i`.
fn wide(i: usize) -> String {
    format!("{i:0>128}")
}

/// The whole heartbeat the agent of `h` would send, with a load at every cap and the
/// longest session.
fn beat(h: &Harness) -> Heartbeat {
    Heartbeat {
        seq: Some(u64::MAX),
        host_id: crate::cli_runtime::HOST.to_owned(),
        epoch: u64::MAX,
        session: wide(1),
        phase: Phase::Serving,
        held: h.life.held(),
        results: h.life.results(),
        taint: None,
        taint_cleared: None,
        tainted: Some(false),
        exclusive_ready: None,
        exclusive_refused: None,
        load: Some(full_load()),
        disk_gib: Some(limits::MAX_DISK_GIB),
        jobs: h.life.held_jobs(),
        retired_jobs: h.life.retired_jobs(),
        log_grant_state: None,
    }
}

/// The serialised size of `beat`, once it has passed the checks the agent makes before it
/// sends one.
fn size(beat: &Heartbeat) -> usize {
    assert_eq!(beat.validate(), Ok(()));
    serde_json::to_vec(beat).unwrap().len()
}

/// A run start at the longest id that the host cannot fit (5 CPUs of 4), so it is refused
/// and its result is held until acknowledged.
fn unfit_run(i: usize) -> Command {
    let mut spec = job(&wide(i));
    spec.lease_id = format!("l{i}");
    spec.limits.cpu_millis = 5000;
    Command::Start(Box::new(spec))
}

/// A job start at the longest id that the host cannot fit, refused as [`unfit_run`] is.
fn unfit_job(i: usize) -> Command {
    let mut spec = start(&wide(i));
    spec.allocation_id = format!("a{i}");
    spec.booked.cpu_millis = 5000;
    job_start(spec)
}

/// Asserts that a server that sends run starts and acknowledges nothing makes the agent take
/// [`LISTABLE`] of them (each refused, and so held with its result) and skip the next as
/// `Full`; that the heartbeat then lists every held run, carries the results oldest first as
/// far as they fit and the rest not at all, and stays within the body the server reads; and
/// that once the carried results are acknowledged, the waiting ones go out and a start is
/// taken again. It catches a beat whose held list and results pass the body limit (the
/// server answers 413 and the agent exits), a result skipped out of order, and a bound that
/// never lets go.
#[test]
fn run_starts_past_what_a_beat_lists_are_skipped_and_results_wait() {
    let mut h = Harness::new("listing-runs");
    for i in 0..LISTABLE {
        let events = h.apply(vec![unfit_run(i)]);
        let said = names(&events);
        assert!(said[0].starts_with("refused "), "run {i}: {said:?}");
    }
    let events = h.apply(vec![unfit_run(LISTABLE)]);
    assert_eq!(names(&events), [format!("skipped {} Full", wide(LISTABLE))]);
    h.life.step();
    let first = beat(&h);
    assert!(
        size(&first) <= HEARTBEAT_BODY_BUDGET,
        "{} bytes",
        size(&first)
    );
    assert_eq!(first.held.len(), LISTABLE);
    let carried: Vec<String> = first.results.iter().map(|r| r.run_id.clone()).collect();
    let oldest: Vec<String> = (0..carried.len()).map(wide).collect();
    assert_eq!(carried, oldest, "the oldest results, in order");
    assert!(
        !carried.is_empty() && carried.len() < LISTABLE,
        "{}",
        carried.len()
    );
    for run_id in &carried {
        h.ack(run_id, "ACCEPTED");
    }
    h.life.step();
    let next = beat(&h);
    assert!(
        size(&next) <= HEARTBEAT_BODY_BUDGET,
        "{} bytes",
        size(&next)
    );
    assert_eq!(next.held.len(), LISTABLE - carried.len());
    assert_eq!(
        next.results[0].run_id,
        wide(carried.len()),
        "the waiting one is next"
    );
    let events = h.apply(vec![unfit_run(LISTABLE)]);
    let said = names(&events);
    assert!(said[0].starts_with("refused "), "taken again: {said:?}");
}

/// Asserts that a server that sends job starts and acknowledges nothing makes the agent take
/// [`LISTABLE`] of them and skip the next as `Full`; that every job is listed in every
/// heartbeat, each a job that ended; that the retired job ids of stops for jobs the agent does
/// not hold go first, oldest first, as far as the entries leave room (62 of 96 at 128-byte
/// ids), and the results after them, oldest first, each beat's carried results the oldest
/// jobs' and the rest listed without one; and that as the server acknowledges what each beat
/// carries, every retired id and every result goes out once, in order, while each heartbeat
/// stays within the body the server reads. It catches a job left out of the listing (the
/// first heartbeat after a hello would lose it), a retired id pushed out by results (the
/// server would lose that job rather than end it as stopped), a result carried after one that
/// waits, and a beat over the limit.
#[test]
fn job_starts_past_what_a_beat_lists_are_skipped_and_results_and_retired_ids_wait() {
    let mut h = Harness::new("listing-jobs");
    for i in 0..LISTABLE {
        let events = h.apply(vec![unfit_job(i)]);
        let said = names(&events);
        assert!(said[0].starts_with("refused "), "job {i}: {said:?}");
    }
    let events = h.apply(vec![unfit_job(LISTABLE)]);
    assert_eq!(names(&events), [format!("skipped {} Full", wide(LISTABLE))]);
    let stops: Vec<String> = (0..RETIRED_LISTED).map(|i| format!("s{i:0>127}")).collect();
    let commands = stops.iter().map(|job_id| Command::JobStop {
        job_id: job_id.clone(),
        reason: None,
    });
    h.apply(commands.collect());
    h.life.step();
    let first = beat(&h);
    assert!(size(&first) <= HEARTBEAT_BODY_BUDGET);
    assert_eq!(first.jobs.len(), LISTABLE, "every job is listed");
    assert!(
        first
            .jobs
            .iter()
            .all(|j| j.state == HeldState::ExitedUnacked)
    );
    // 147 entries of 167 bytes leave 8,219 of the 32,768: 62 retired ids of 131.
    assert_eq!(first.retired_jobs, stops[..62]);
    assert!(
        first.jobs.iter().all(|j| j.result.is_none()),
        "no room left"
    );
    let (mut acked, mut retired) = (Vec::new(), Vec::new());
    loop {
        let sent = beat(&h);
        assert!(
            size(&sent) <= HEARTBEAT_BODY_BUDGET,
            "{} bytes",
            size(&sent)
        );
        let left: Vec<String> = (acked.len()..LISTABLE).map(wide).collect();
        let listed: Vec<String> = sent.jobs.iter().map(|j| j.job_id.clone()).collect();
        assert_eq!(listed, left, "every job not yet acknowledged is listed");
        let with = sent.jobs.iter().take_while(|j| j.result.is_some()).count();
        assert!(sent.jobs[with..].iter().all(|j| j.result.is_none()));
        let carried: Vec<String> = sent.jobs[..with].iter().map(|j| j.job_id.clone()).collect();
        if carried.is_empty() && sent.retired_jobs.is_empty() {
            break;
        }
        let acks: Vec<_> = carried
            .iter()
            .map(|id| json!({ "job_id": id, "ack": "ACCEPTED" }))
            .collect();
        let reply = json!({ "job_acks": acks, "retired_acks": sent.retired_jobs });
        h.life.apply(&reply_json(&reply));
        h.life.step();
        acked.extend(carried);
        retired.extend(sent.retired_jobs);
    }
    assert_eq!(acked, (0..LISTABLE).map(wide).collect::<Vec<_>>());
    assert_eq!(retired, stops);
    assert!(h.life.held_jobs().is_empty() && h.life.retired_jobs().is_empty());
}
