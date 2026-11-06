//! The smoke family: small scenarios that prove the whole machine works, the real lease
//! server, registering agents on the real wire and the lease client, under every
//! invariant of `Invariant::ALL` (the step invariants after every step, the end
//! invariants once each scenario has settled). The scenarios are in `cell/burst.rs`,
//! `cell/lossy_wire.rs` and `cell/partition.rs`, which the sweep (`tests/sweep.rs`) runs too
//! over many more seeds.
//!
//! Each states only what the server does today and the design also promises; none pins
//! a behaviour that section 8 of the design marks Open.
//!
//! Sizes, in agent requests a seed (heartbeats, with the hellos besides):
//! - a hundred hosts: 12,000 (`cell/burst.rs`), over [`BURST_SEEDS`] seeds;
//! - the lossy wire: about 8,400 (`cell/lossy_wire.rs`), over [`LOSSY_SEEDS`] seeds;
//! - the partition: about 7,700 (`cell/partition.rs`), over [`CUT_SEEDS`] seeds.

#[path = "cell/burst.rs"]
mod burst;
#[path = "cell/lossy_wire.rs"]
mod lossy_wire;
#[path = "cell/partition.rs"]
mod partition;

/// The hundred hosts' seeds.
const BURST_SEEDS: u64 = 4;

/// The lossy wire's seeds.
const LOSSY_SEEDS: u64 = 8;

/// The partition's seeds.
const CUT_SEEDS: u64 = 8;

/// Proves the machine at size with no fault: 400 runs of 300 s on a hundred hosts that
/// beat every 30 s all succeed, each launched once, no host booked past what it holds,
/// and nothing left booked or held at the end. Red for a server that double-books a host
/// (`no-double-booking`: two runs of one GPU on a host of one), one that relaunches a run
/// on a resent heartbeat (`at-most-one-launch`), one that leaves a booking after its
/// release (`bookings-match-agents`), and one that loses or fails runs (the expected
/// count of successes).
#[test]
fn a_hundred_hosts_settle_four_hundred_runs() {
    burst::a_hundred_hosts_settle_four_hundred_runs().run_seeds(BURST_SEEDS);
}

/// Proves that the client's work settles while one message in ten between the agents and
/// the server is lost for the first 900 s: every item ends exactly once, no run is launched
/// twice, and nothing is left booked or held once the loss has ended; its witness shows a
/// result was sent again because its acknowledgement was lost. Red for a server that
/// relaunches a run it hears about again (`at-most-one-launch`), one that double-books a
/// host when an answer is lost (`no-double-booking`), and one that keeps a booking after
/// its release (`bookings-match-agents`). The scenario is in `cell/lossy_wire.rs`.
#[test]
fn a_lossy_agent_wire_settles_every_run() {
    lossy_wire::a_lossy_agent_wire_settles_every_run().run_seeds(LOSSY_SEEDS);
}

/// Proves that work settles across a partition: ten agents drawn from the seed are cut
/// off for 60 s, longer than the grace, while they run, and healed. What the server does
/// with a run on a host that went lost and came back may differ from the design (section
/// 8, rows 4 and 35), so the scenario states only what both promise, through the
/// invariants: every item is terminal or given up, nothing is launched twice, nothing is
/// double-booked, and nothing is left booked once the cut has healed. Red for a server
/// that books a lost host's GPU to a second lease while its run still runs
/// (`no-double-booking`), one that starts a run again on a host that comes back
/// (`at-most-one-launch`), and one that keeps a lost host's booking after its lease is
/// released (`bookings-match-agents`, `settles-when-quiet`).
#[test]
fn a_partition_of_ten_agents_that_heals_settles_every_run() {
    partition::a_partition_of_ten_agents_that_heals_settles_every_run().run_seeds(CUT_SEEDS);
}
