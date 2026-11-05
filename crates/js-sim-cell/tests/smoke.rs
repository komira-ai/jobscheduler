//! The smoke family: small scenarios that prove the whole machine works, the real lease
//! server, registering agents on the real wire and the lease client, under every
//! invariant of `Invariant::ALL` (the step invariants after every step, the end
//! invariants once each scenario has settled).
//!
//! Each states only what the server does today and the design also promises; none pins
//! a behaviour that section 8 of the design marks Open.
//!
//! Sizes, in agent requests a seed (heartbeats, with the hellos besides):
//! - a hundred hosts: [`BURST_HOSTS`] beating every [`BURST_INTERVAL`] for
//!   [`BURST_SETTLE`] make 12,000, over [`BURST_SEEDS`] seeds;
//! - the lossy wire: about 8,400 (`cell/lossy_wire.rs`), over [`LOSSY_SEEDS`] seeds;
//! - the partition: [`CUT_HOSTS`] beating every 5 s until [`CUT_AT`], [`CUT_FOR`] and
//!   [`CUT_SETTLE`], 1,920 s, make about 7,700, over [`CUT_SEEDS`] seeds.

#[path = "cell/lossy_wire.rs"]
mod lossy_wire;

use std::time::Duration;

use js_sim_cell::actions::{drawn_agents, partition_agents};
use js_sim_cell::{Expect, HowEnded, Scenario, ServerSpec, Shape, Work};

/// A hundred hosts: one GPU each, so a hundred runs at a time.
const BURST_HOSTS: usize = 100;
const BURST_RUNS: u32 = 400;
const BURST_RUN: Duration = Duration::from_secs(300);
const BURST_GIVE_UP: Duration = Duration::from_secs(2_400);
const BURST_INTERVAL: Duration = Duration::from_secs(30);
const BURST_GRACE: Duration = Duration::from_secs(90);
const BURST_SETTLE: Duration = Duration::from_secs(3_600);
const BURST_SEEDS: u64 = 4;

/// The lossy wire's seeds.
const LOSSY_SEEDS: u64 = 8;

/// The partition: ten of twenty agents, drawn from the seed, cut for longer than the
/// grace (15 s, the server's default) and healed.
const CUT_HOSTS: usize = 20;
const CUT_RUNS: u32 = 60;
const CUT_RUN: Duration = Duration::from_secs(120);
const CUT_GIVE_UP: Duration = Duration::from_secs(1_800);
const CUT_AGENTS: usize = 10;
const CUT_AT: Duration = Duration::from_secs(60);
const CUT_FOR: Duration = Duration::from_secs(60);
const CUT_SETTLE: Duration = Duration::from_secs(1_800);
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
    Scenario::new("a_hundred_hosts_settle_four_hundred_runs")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(
            ServerSpec::registered_pool("burst", Shape::gpu_host(), BURST_HOSTS)
                .heartbeat(BURST_INTERVAL, BURST_GRACE),
        )
        .hosts("burst", BURST_HOSTS)
        .work(
            BURST_RUNS,
            Work::run_for(BURST_RUN).give_up_after(BURST_GIVE_UP),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            BURST_RUNS as usize,
            HowEnded::Succeeded,
        ))
        .settle_within(BURST_SETTLE)
        .run_seeds(BURST_SEEDS);
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
    Scenario::new("a_partition_of_ten_agents_that_heals_settles_every_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            CUT_HOSTS,
        ))
        .hosts("burst", CUT_HOSTS)
        .work(CUT_RUNS, Work::run_for(CUT_RUN).give_up_after(CUT_GIVE_UP))
        .at(CUT_AT, partition_agents(drawn_agents(CUT_AGENTS), CUT_FOR))
        .expect(Expect::all_items_terminal())
        .settle_within(CUT_SETTLE)
        .run_seeds(CUT_SEEDS);
}
