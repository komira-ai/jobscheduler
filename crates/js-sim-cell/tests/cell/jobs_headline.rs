//! The headline over jobs, shared by the job scenario family (`tests/jobs.rs`), which runs
//! it over sixteen seeds, and the replay tests, which run a seed twice and pin its trace
//! hash in the golden file (family `jobs_scenarios`).
//!
//! Size: [`HOSTS`] agents beating every [`INTERVAL`] until [`KILL_AT`] plus
//! [`REPLACE_AFTER`] plus [`SETTLE`], 770 s, make about 510 heartbeats a seed; the
//! hellos at the start, and those of the two new processes, refused once a second until
//! their hosts read lost (about 70 s each), add about 160, for about 670. It is the
//! two-hundred-host headline at a tenth of its hosts, jobs, cut and killed agents, on the
//! same timeline: it runs in every pull request's `test` and `coverage` jobs, where
//! sixteen seeds of two hundred hosts take far longer than either job allows.

use std::time::Duration;

use js_sim_cell::actions::{drawn_agents, kill_agents_mid_job, partition_agents};
use js_sim_cell::{Expect, Scenario, ServerSpec, Shape, Work};

/// Twenty hosts beating every 30 s, held lost after 90 s of silence.
const HOSTS: usize = 20;
const INTERVAL: Duration = Duration::from_secs(30);
const GRACE: Duration = Duration::from_secs(90);

/// Forty jobs of 300 s, which the client gives up on only after 2,400 s.
const JOBS: u32 = 40;
const RUN: Duration = Duration::from_secs(300);
const GIVE_UP: Duration = Duration::from_secs(2_400);

/// Four agents, drawn from the seed, cut off for 30 s while their jobs run: shorter than
/// the grace, so their hosts stay live.
const CUT: usize = 4;
const CUT_AT: Duration = Duration::from_secs(60);
const CUT_FOR: Duration = Duration::from_secs(30);

/// Two agents killed mid-job, once every agent has listed its jobs again after the cut,
/// and replaced 20 s later.
const KILLED: usize = 2;
const KILL_AT: Duration = Duration::from_secs(150);
const REPLACE_AFTER: Duration = Duration::from_secs(20);

/// How long the cell runs after the replacement, before the end check: the new processes'
/// hosts read lost (90 s), their hellos are taken, the jobs they no longer list end, and
/// every other job runs out its 300 s, all by about 400 s.
const SETTLE: Duration = Duration::from_secs(600);

/// The headline over jobs: twenty hosts run forty jobs of 300 s; at 60 s four agents,
/// drawn from the seed, are cut off for 30 s, and at 150 s two agents running
/// jobs every heartbeat of theirs had listed are killed and, 20 s later, replaced by new
/// processes that hold nothing. It states, beside every invariant (the five job invariants
/// among them, and `no-double-booking`, which holds each host to its capacity at every
/// step), that every job reaches one terminal state, on the client's side
/// (`all-items-terminal`) and in the server's table (`jobs-read`, `jobs-ended`), that no
/// job is launched twice under any allocation (`each-job-launched-once`), and that once
/// every job is terminal no allocation is left booked or releasing (`no-allocation-live`).
pub fn the_headline_over_jobs() -> Scenario {
    let spec =
        ServerSpec::registered_pool("burst", Shape::gpu_host(), HOSTS).heartbeat(INTERVAL, GRACE);
    let jobs = JOBS as usize;
    Scenario::new("the_headline_over_jobs")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", HOSTS)
        .work(JOBS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .at(CUT_AT, partition_agents(drawn_agents(CUT), CUT_FOR))
        .at(KILL_AT, kill_agents_mid_job(KILLED, REPLACE_AFTER))
        .expect(Expect::all_items_terminal())
        .expect(Expect::jobs_read(jobs))
        .expect(Expect::jobs_ended(jobs))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::no_allocation_live())
        .settle_within(SETTLE)
}
