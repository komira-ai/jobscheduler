//! The whole-host family: the server's whole-host path (a whole-node request under a
//! request id, its wait, the commit, the agent's prepare and ready report, the lease's
//! run, its release and the host's restore) over many hosts, under every invariant of
//! `Invariant::ALL`, once per value of the server's `--placement`, so that a placement
//! that moves the path onto another engine is held to the registry's answer.
//!
//! Each scenario states only what the server does today and the design also promises:
//! it pins neither the order in which the wait list serves (design section 8, row 11) nor
//! anything section 8 marks Open. The scenarios are in `cell/whole_host.rs`, shared with
//! the replay tests that pin their trace hashes.
//!
//! The negative cases show two whole-host invariants failing on a history the real cell
//! made, with one fact changed by hand: an agent that reports ready for another lease, and
//! a server that drains by cancelling.
//!
//! Sizes, in agent requests a seed: the fifty requests, about 4,800 heartbeats
//! (`cell/whole_host.rs`), over [`SEEDS`] seeds under each placement; a negative case,
//! one or two hosts for at most [`LOOK_AT`].

#[path = "cell/whole_host.rs"]
mod whole_host;

use std::path::Path;
use std::time::Duration;

use clap::ValueEnum;
use js_lease_server::Placement;
use js_sim::Faults;
use js_sim_cell::{
    Cell, Checker, ClientEvent, Invariant, ItemId, Observations, Prepare, RunId, ServerSpec, Setup,
    Shape, StopCommand, StopSeen, Touched, Violation, Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId};

/// How many seeds the scenario runs under each placement.
const SEEDS: u64 = 16;

/// How long a negative case runs its cell: past the commit, the ready report, the grant
/// and the start of the run of [`NEGATIVE_RUN`], which is still running then.
const LOOK_AT: FarmTime = FarmTime::from_millis(60_000);
const NEGATIVE_RUN: Duration = Duration::from_secs(600);

/// Proves the whole-host path over twenty hosts under every placement the server accepts
/// (`Placement::value_variants()`): fifty whole-node requests are each held once and
/// released, and every host is serving at the end (design section 5.4 as built, through
/// the wait list of section 8, rows 11 and 12, pinning neither; the scenario's own doc
/// says what turns it red).
#[test]
fn fifty_whole_host_requests_over_twenty_hosts_are_each_held_once_and_released() {
    for placement in Placement::value_variants() {
        whole_host::fifty_whole_host_requests(*placement).run_seeds(SEEDS);
    }
}

/// A cell of one host per entry of `prepares` (agent `i` answering a prepare as
/// `prepares[i]` says) in a whole-node pool under `placement`, with one whole-host item
/// of [`NEGATIVE_RUN`] submitted at time zero, run to [`LOOK_AT`].
fn negative_cell(name: &str, placement: Placement, prepares: &[Prepare]) -> Cell {
    let spec = ServerSpec::whole_node_pool("burst", Shape::gpu_host(), prepares.len());
    let spec = spec.placement(placement);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    let dir = TestDir::under(root, &format!("{name}-{}", whole_host::spelled(placement)));
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

/// The one lease the server's listing books on `h000`.
fn booked(observations: &Observations) -> LeaseId {
    let listed = &observations.server.hosts[&h000()];
    assert_eq!(listed.leases.len(), 1, "one lease is booked on h000");
    listed.leases[0]
}

/// Shows a defective agent failing `HeldMeansReady` on the real server's history: an
/// agent that, told to prepare for its lease, reports ready for another lease id. The
/// server keeps the lease pending (it is never granted, so the run alone shows no
/// violation); the same history with the grant the client would have heard written in
/// by hand is flagged, because no ready report names that lease. Its correct twin, an
/// agent that reports ready for the lease it was told, is granted for real and passes.
/// Catches an invariant that takes any ready report of the host as the ready report of
/// the lease, and one that reads what the server granted instead of what the agent sent.
#[test]
fn a_defective_agent_that_reports_ready_for_the_wrong_lease_fails_held_means_ready() {
    for placement in Placement::value_variants() {
        let wrong = negative_cell("wrong_lease", *placement, &[Prepare::ReadyForAnotherLease]);
        let seen = wrong.observations().clone();
        let lease = booked(&seen);
        let named: Vec<&str> = seen.ready.iter().map(|r| r.lease.as_str()).collect();
        assert!(!named.is_empty(), "the defective agent reported ready");
        assert!(named.iter().all(|named| *named == format!("{lease}0")));
        let granted = seen
            .with_event(ItemId(0), LOOK_AT, queued(lease))
            .with_event(ItemId(0), LOOK_AT, ClientEvent::Placed { host: h000() });
        flags_only(&flagged(&granted), Invariant::HeldMeansReady, "item0");

        let right = negative_cell("right_lease", *placement, &[Prepare::default()]);
        let seen = right.observations();
        let placed = seen.items[&ItemId(0)].heard.iter();
        let placed = placed.filter(|heard| matches!(heard.event, ClientEvent::Placed { .. }));
        assert_eq!(placed.count(), 1, "the correct agent's lease is granted");
        assert_eq!(flagged(seen), Vec::new());
    }
}

/// `Queued` of `lease` and its run `r`.
fn queued(lease: LeaseId) -> ClientEvent {
    ClientEvent::Queued {
        lease,
        run: RunId::new("r"),
    }
}

/// Shows a defective server view failing `DrainKillsNothing` on the real server's
/// history, with no change to the server: the whole-host item's run is running on
/// `h000`; the same history with the listing setting `h000` aside for another request
/// and a cancel of that run reaching the agent, which the client never asked for, is
/// flagged; with the client's cancel asked first it is not. Catches an invariant that
/// misses a drain done by cancelling, and one that flags the client's own cancel.
#[test]
fn a_defective_server_view_that_drains_by_cancel_fails_drain_kills_nothing() {
    for placement in Placement::value_variants() {
        let cell = negative_cell("drain_by_cancel", *placement, &[Prepare::default()]);
        let seen = cell.observations().clone();
        let truth = &seen.agents[&h000()];
        let (run, running) = truth.running.iter().next().expect("the run is running");
        let cancel = StopSeen {
            host: h000(),
            run: run.clone(),
            lease: running.lease,
            at: LOOK_AT,
            command: StopCommand::Cancel,
            wall_ends_at: FarmTime::from_millis(u64::MAX),
        };
        let drained = seen
            .clone()
            .with_reservation(h000(), "another-request", None)
            .with_stop(cancel);
        let found = flagged(&drained);
        flags_only(&found, Invariant::DrainKillsNothing, "another-request");
        let asked = drained.with_cancel(ItemId(0), LOOK_AT);
        assert_eq!(flagged(&asked), Vec::new());
        assert_eq!(flagged(&seen), Vec::new());
    }
}
