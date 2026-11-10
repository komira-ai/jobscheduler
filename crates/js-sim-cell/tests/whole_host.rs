//! The whole-host family: the server's whole-host path for jobs (a job that asks for a
//! whole host, its wait in the queue, its placement on a quiet host, the start, the run and
//! the host's return to service) over many hosts, under every invariant of
//! `Invariant::ALL`.
//!
//! The scenario states only what the server does today and the design also promises: it
//! pins neither the order in which the queue serves (design section 8, row 11) nor
//! anything section 8 marks Open. It is in `cell/whole_host.rs`, shared with the replay
//! tests that pin its trace hashes.
//!
//! The negative cases show the two whole-host invariants that read a job's history failing
//! on a history the real cell made, with one fact changed by hand: an agent that reports
//! ready for another allocation, and a server that drains by stopping a job.
//!
//! Sizes, in agent requests a seed: the fifty jobs, about 4,800 heartbeats
//! (`cell/whole_host.rs`), over [`SEEDS`] seeds; a negative case, one host for at most
//! [`LOOK_AT`].

#[path = "cell/whole_host.rs"]
mod whole_host;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use js_jobs::{JobId, JobState};
use js_sim::Faults;
use js_sim_cell::{
    Cell, Checker, Invariant, ItemId, JobReply, Observations, Prepare, ServerSpec, Setup, Shape,
    Touched, Violation, Work,
};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::sched::FarmTime;

/// How many seeds the scenario runs.
const SEEDS: u64 = 16;

/// How long a negative case runs its cell: past the placement, the ready report and the
/// start of the job of [`NEGATIVE_RUN`], which is still running then.
const LOOK_AT: FarmTime = FarmTime::from_millis(60_000);
const NEGATIVE_RUN: Duration = Duration::from_secs(600);

/// Proves the whole-host path over twenty hosts: fifty whole-host jobs are each placed once
/// and released, and every host is serving at the end (design section 5.4 as built, through
/// the queue of section 8, rows 11 and 12, pinning neither; the scenario's own doc says
/// what turns it red).
#[test]
fn fifty_whole_host_requests_over_twenty_hosts_are_each_held_once_and_released() {
    whole_host::fifty_whole_host_requests().run_seeds(SEEDS);
}

/// A cell of one host per entry of `prepares` (agent `i` answering a prepare as
/// `prepares[i]` says) in a whole-node pool, with one whole-host job of [`NEGATIVE_RUN`]
/// submitted at time zero, run to [`LOOK_AT`].
fn negative_cell(name: &str, prepares: &[Prepare]) -> Cell {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), prepares.len());
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    let dir = TestDir::under(root, name);
    let mut cell = Cell::new(Setup::new(1), spec, dir, Faults::default()).expect("starts");
    for (index, prepare) in prepares.iter().enumerate() {
        cell.add_agent().expect("an agent name is a host id");
        cell.set_prepare(index, *prepare);
    }
    cell.submit(&Work::whole_host(NEGATIVE_RUN));
    cell.run_until(LOOK_AT)
        .expect("no invariant fails on the real history");
    cell
}

/// A fresh checker's step over everything `observations` hold: the invariants it names.
fn flagged(observations: &Observations) -> Vec<Violation> {
    Checker::new().step(observations, &Touched::everything_in(observations))
}

/// Asserts that `found` is exactly one violation of `invariant` whose line contains
/// `naming`.
fn flags_only(found: &[Violation], invariant: Invariant, naming: &str) {
    let lines: Vec<String> = found.iter().map(ToString::to_string).collect();
    assert_eq!(found.len(), 1, "{lines:?}");
    assert_eq!(found[0].invariant, invariant, "{lines:?}");
    assert!(lines[0].contains(naming), "{lines:?}");
}

/// The host `h000`.
fn h000() -> HostId {
    HostId::new("h000").expect("a host id")
}

/// The job of item 0, `sim-job-0`.
fn job0() -> JobId {
    JobId::parse("sim-job-0").expect("a job id")
}

/// Shows a defective agent failing `HeldMeansReady` on the real server's history: an agent
/// that, told to prepare for its job's allocation, reports ready for another allocation id.
/// The server keeps the job placed (it never starts, so the run alone shows no violation);
/// the same history with the read of the job starting that the client would have heard
/// written in by hand is flagged, because no ready report names that allocation. Its
/// correct twin, an agent that reports ready for the allocation it was told, starts the job
/// for real and passes. Catches an invariant that takes any ready report of the host as the
/// ready report of the allocation, and one that reads what the server started instead of
/// what the agent sent.
#[test]
fn a_defective_agent_that_reports_ready_for_the_wrong_allocation_fails_held_means_ready() {
    let wrong = negative_cell("wrong_allocation", &[Prepare::ReadyForAnotherLease]);
    let seen = wrong.observations().clone();
    let booked = seen.allocation_reads.last().expect("the job was placed");
    let allocation = booked.row.allocation.to_string();
    let named: Vec<&str> = seen.ready.iter().map(|r| r.lease.as_str()).collect();
    assert!(!named.is_empty(), "the defective agent reported ready");
    assert!(named.iter().all(|named| *named == format!("{allocation}0")));
    assert_eq!(
        seen.job_reads.last().map(|read| read.state),
        Some(JobState::Placed)
    );
    let started = seen.with_job_read(job0(), JobState::Starting, LOOK_AT);
    flags_only(&flagged(&started), Invariant::HeldMeansReady, "sim-job-0");

    let right = negative_cell("right_allocation", &[Prepare::default()]);
    let seen = right.observations();
    let states: Vec<JobState> = seen.job_reads.iter().map(|read| read.state).collect();
    assert!(states.contains(&JobState::Starting), "{states:?}");
    assert_eq!(flagged(seen), Vec::new());
}

/// Shows a defective server view failing `DrainKillsNothing` on the real server's history,
/// with no change to the server: the whole-host job runs on `h000`; the same history with
/// the listing setting `h000` aside for another request and a stop of that job reaching the
/// agent, which the client never asked for, is flagged; with the client's cancel asked first
/// it is not, and the plain history is not. Catches an invariant that misses a drain done by
/// stopping a job, and one that flags the client's own cancel.
#[test]
fn a_defective_server_view_that_drains_by_stopping_a_job_fails_drain_kills_nothing() {
    let cell = negative_cell("drain_by_stop", &[Prepare::default()]);
    let seen = cell.observations().clone();
    let truth = &seen.agents[&h000()];
    let running: Vec<&JobId> = truth.jobs.keys().collect();
    assert_eq!(running, [&job0()], "the job is running on h000");
    let stop = JobReply {
        host: h000(),
        at: LOOK_AT,
        beat_at: LOOK_AT,
        listed: BTreeSet::from([job0()]),
        starts: Vec::new(),
        stops: BTreeSet::from([job0()]),
        acks: Vec::new(),
    };
    let drained = seen
        .clone()
        .with_reservation(h000(), "another-request", None)
        .with_job_reply(stop);
    let found = flagged(&drained);
    flags_only(&found, Invariant::DrainKillsNothing, "sim-job-0");
    let asked = drained.with_cancel(ItemId(0), LOOK_AT);
    assert_eq!(flagged(&asked), Vec::new());
    assert_eq!(flagged(&seen), Vec::new());
}
