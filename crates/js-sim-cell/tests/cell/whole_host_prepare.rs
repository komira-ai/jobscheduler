//! The scenarios of the whole-host prepare family (`whole_host_prepare.rs`): what the
//! server does with a whole-host job whose host does not become ready, or whose job is
//! cancelled, or whose agent restarts, while the host prepares.
//!
//! The deadline is the server's own, [`PREPARE_DEADLINE`] (the dedicated pool's default,
//! `WholeNodeConfig::dedicated`), and the host stays out of the job's next placement for
//! [`BACKOFF`] after the failure (`reserve_backoff`'s default); a job fails to start after
//! [`MAX_REPLACE`] failed prepares. A scenario that waits out a deadline runs for virtual
//! time of that order.
//!
//! Sizes, in agent requests a seed (heartbeats every 5 s, the server's default, with the
//! hellos besides), each far under the 30,000 a seed of the cell's sizing rule:
//! - the silent hosts: [`SILENT_HOSTS`] for [`SILENT_SETTLE`], 800 s, make 320, or
//!   [`SCALE_HOSTS`], 8,000;
//! - the hosts that never answer: [`LOST_HOSTS`] for [`LOST_SETTLE`], 2,400 s, make 1,440, or
//!   [`SCALE_HOSTS`], 24,000;
//! - the one host that never answers: one host for [`BACKOFF_SETTLE`], 3,000 s, make 600;
//! - a refusal: [`REFUSAL_HOSTS`] for [`REFUSAL_SETTLE`], 900 s, make 720;
//! - the race: [`RACE_HOSTS`] for [`RACE_SETTLE`], 1,800 s, make 720 for each of the
//!   [`RACE_SWEEP`] scenarios;
//! - the restarts of a committed host: [`COMMIT_HOSTS`] for [`COMMIT_SETTLE`], 900 s, make
//!   540, and the restart of a preparing host: one host for [`RESTART_SETTLE`], 1,200 s,
//!   make 240;
//! - the cancels: one host for [`CANCEL_SETTLE`], 1,200 s, make 240.

use std::ops::RangeInclusive;
use std::time::Duration;

use js_agent_wire::RefusalReason;
use js_jobs::{AllocationStep, JobState, StartFailure};
use js_sim_cell::actions::{
    REFUSAL_REASONS, agent_prepare_delay, agent_refuses_prepare, agent_silent_on_prepare, agents,
    cancel_when_preparing, replace_agents,
};
use js_sim_cell::{Expect, HowEnded, ItemId, Scenario, ServerSpec, Shape, Work};

/// How long the server gives a host to become ready, from the placement: the default of a
/// dedicated whole-node pool (`WholeNodeConfig::dedicated`, 600,000 ms), which the pool file
/// no longer changes.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(600);

/// How long a host whose prepare failed stays out of the job's next placement: the default
/// `reserve_backoff` (`DEFAULT_RESERVE_BACKOFF_MS`, 300,000 ms).
pub const BACKOFF: Duration = Duration::from_secs(300);

/// How many failed prepares end a job as `exclusive-not-ready` (`max_replace`'s default).
pub const MAX_REPLACE: usize = 3;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A whole-node pool of `hosts` hosts.
fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::cpu_host(), hosts)
}

/// How many hosts the scaled worlds of the silent-host scenarios have: the job's tries meet
/// many idle hosts, all silent.
pub const SCALE_HOSTS: usize = 50;

/// The silent hosts: two hosts, silent from the start, and one job.
pub const SILENT_HOSTS: usize = 2;
const SILENT_RUN: Duration = Duration::from_secs(10);
/// When the agents are made ready, after the first try failed and while the second waits.
const SILENT_READY_AT: Duration = Duration::from_secs(700);
const SILENT_SETTLE: Duration = Duration::from_secs(800);

/// Proves that a silent host fails the prepare, and that agents made ready take the next
/// try. `hosts` hosts ([`SILENT_HOSTS`], or [`SCALE_HOSTS`] idle ones) whose agents never
/// answer a prepare, and one whole-host job at time
/// zero. Its first try sits on a host until the server's deadline, [`PREPARE_DEADLINE`],
/// ends it; the job goes back to the queue with that host barred for [`BACKOFF`], so its
/// second try is on another host, which is silent too until the agents are made ready
/// at [`SILENT_READY_AT`] (the action also names an agent the cell does not have, which
/// changes nothing), and the next heartbeat reports ready: the job starts, runs and
/// succeeds on its second allocation. The job is placed again, it is not placed on the
/// first host again within the bar (counted from the first try's creation, the failure
/// being at least [`PREPARE_DEADLINE`] later), and the hosts serve at the end. Red for a
/// server that leaves the job on the silent host past its deadline (one allocation, the
/// job not terminal), ends the job at the first failed prepare, or places it on the
/// barred host again.
pub fn a_silent_host(hosts: usize) -> Scenario {
    let job = ItemId(0);
    let name = format!("a_silent_host_fails_the_prepare_and_the_job_goes_to_another_host_{hosts}");
    Scenario::new(&name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(hosts))
        .hosts("burst", hosts)
        .work(1, Work::whole_host(SILENT_RUN))
        .at(Duration::ZERO, agent_silent_on_prepare(agents(0..hosts)))
        .at(
            SILENT_READY_AT,
            agent_prepare_delay(agents(0..hosts + 1), Duration::ZERO),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::job_state(job, JobState::Succeeded))
        .expect(Expect::allocations_of(job, 2))
        .expect(Expect::jobs_placed_again(1))
        .expect(Expect::kept_off_a_failed_host(PREPARE_DEADLINE + BACKOFF))
        .expect(Expect::held_and_released(1))
        .expect(Expect::every_host_serving())
        .settle_within(SILENT_SETTLE)
}

/// The hosts that never answer: three hosts, silent for good, and one job.
pub const LOST_HOSTS: usize = 3;
const LOST_SETTLE: Duration = Duration::from_secs(2_400);

/// Proves that a job is terminal after [`MAX_REPLACE`] silent prepares: `hosts` hosts
/// ([`LOST_HOSTS`], or [`SCALE_HOSTS`]) whose
/// agents never answer, one whole-host job. Each try ends at the deadline and puts the job
/// back in the queue, the third ends it: it fails to start with `exclusive-not-ready`
/// (the client hears it fail), it was placed three times, and every host serves again.
/// Red for a server that tries a fourth time, ends the job before the third failed
/// prepare, or leaves its allocation booked or a host preparing.
pub fn a_job_is_terminal_after_max_replace_silent_prepares(hosts: usize) -> Scenario {
    let job = ItemId(0);
    let name = format!("a_job_is_terminal_after_max_replace_silent_prepares_{hosts}");
    let ended = JobState::FailedToStart(StartFailure::ExclusiveNotReady);
    Scenario::new(&name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(hosts))
        .hosts("burst", hosts)
        .work(1, Work::whole_host(SILENT_RUN))
        .at(Duration::ZERO, agent_silent_on_prepare(agents(0..hosts)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Failed))
        .expect(Expect::job_state(job, ended))
        .expect(Expect::allocations_of(job, MAX_REPLACE))
        .expect(Expect::no_allocation_live())
        .expect(Expect::every_host_serving())
        .settle_within(LOST_SETTLE)
}

/// The back-off: one host, silent for good, and one job.
const BACKOFF_SETTLE: Duration = Duration::from_secs(3_000);

/// Proves that a silent host is left out of the retry for the back-off: one host whose
/// agent never answers, one whole-host job. The job has no other host to go to, so each
/// retry waits for the bar to end: it is placed again on the same host only [`BACKOFF`]
/// after the deadline failed the try before, never sooner (the second allocation is created
/// at least [`PREPARE_DEADLINE`] plus [`BACKOFF`] after the first, the third after the
/// second), and fails to start with `exclusive-not-ready` after [`MAX_REPLACE`] tries. Red
/// for a server that places the job on the host at once after a failed prepare (an
/// allocation sooner than the bar), or never places it again.
pub fn a_silent_host_is_left_out_of_the_retry_for_the_backoff() -> Scenario {
    let job = ItemId(0);
    let ended = JobState::FailedToStart(StartFailure::ExclusiveNotReady);
    Scenario::new("a_silent_host_is_left_out_of_the_retry_for_reserve_backoff")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::whole_host(SILENT_RUN))
        .at(Duration::ZERO, agent_silent_on_prepare(agents(0..1)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Failed))
        .expect(Expect::job_state(job, ended))
        .expect(Expect::allocations_of(job, MAX_REPLACE))
        .expect(Expect::kept_off_a_failed_host(PREPARE_DEADLINE + BACKOFF))
        .expect(Expect::no_job_launched())
        .expect(Expect::every_host_serving())
        .settle_within(BACKOFF_SETTLE)
}

/// The refusal: four hosts, the first refusing, four jobs.
pub const REFUSAL_HOSTS: usize = 4;
const REFUSAL_RUN: Duration = Duration::from_secs(60);
const REFUSAL_SETTLE: Duration = Duration::from_secs(900);
/// How long the client of a refusal scenario waits for a job before it gives up on it.
const REFUSAL_PATIENCE: Duration = Duration::from_secs(300);

/// Proves that a refused prepare is answered as the reason says: four hosts, the first
/// one's agent refuses every prepare with `reason`, and four whole-host jobs of
/// [`REFUSAL_RUN`] at time zero, so that one job is placed on each host. The job on the
/// refusing host goes back to the queue and the other three succeed.
///
/// For every reason but `host-unsafe` the refused job is placed again on a host that is
/// free once one of the others ended, never on the refusing host within the bar: every
/// job succeeds, exactly one is placed again, and at the end every host serves. A
/// `host-unsafe` refusal taints the host it came from instead, which is then out of
/// service for good (`serving_but`); the three other jobs succeed, and the client gives up
/// on the job that was refused after [`REFUSAL_PATIENCE`]: where the refused job goes next
/// is not stated.
///
/// Red for a server that ends the job at the refusal, places it on the refusing host again
/// within [`BACKOFF`], or does not taint a host that refused as unsafe (or taints one that
/// did not).
pub fn a_refusal(reason: RefusalReason) -> Scenario {
    let unsafe_host = reason == RefusalReason::HostUnsafe;
    let jobs = REFUSAL_HOSTS;
    let run = Work::whole_host(REFUSAL_RUN).give_up_after(REFUSAL_PATIENCE);
    let scenario = Scenario::new("a_refused_prepare_goes_elsewhere_for_each_refusal_reason")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(REFUSAL_HOSTS))
        .hosts("burst", REFUSAL_HOSTS)
        .work(jobs as u32, run)
        .at(Duration::ZERO, agent_refuses_prepare(agents(0..1), reason))
        .expect(Expect::all_items_terminal())
        .settle_within(REFUSAL_SETTLE);
    if unsafe_host {
        return scenario
            .expect(Expect::terminal_count(jobs - 1, HowEnded::Succeeded))
            .expect(Expect::terminal_count(1, HowEnded::GaveUp))
            .expect(Expect::serving_but(&[0]));
    }
    scenario
        .expect(Expect::terminal_count(jobs, HowEnded::Succeeded))
        .expect(Expect::held_and_released(jobs))
        .expect(Expect::jobs_placed_again(1))
        .expect(Expect::kept_off_a_failed_host(BACKOFF))
        .expect(Expect::every_host_serving())
}

/// Every refusal scenario, one for each reason of [`REFUSAL_REASONS`].
pub fn every_refusal() -> Vec<(RefusalReason, Scenario)> {
    REFUSAL_REASONS
        .into_iter()
        .map(|reason| (reason, a_refusal(reason)))
        .collect()
}

/// The race: two hosts, the first slow to prepare, two jobs.
pub const RACE_HOSTS: usize = 2;
const RACE_RUN: Duration = Duration::from_secs(10);
const RACE_SETTLE: Duration = Duration::from_secs(1_800);

/// The first host's preparation takes the server's deadline plus this many seconds (so a
/// negative step is a host a little quicker than the deadline). The agent is told of the
/// job a heartbeat after the placement and reports a heartbeat after its delay ended, so
/// the ready report meets the deadline's tick somewhere in the range.
pub const RACE_SWEEP: RangeInclusive<i64> = -10..=2;

/// Proves that a ready report racing the prepare deadline never double-books: two hosts,
/// two whole-host jobs of [`RACE_RUN`] at time zero, and the first host's agent takes
/// [`PREPARE_DEADLINE`] plus `step` seconds to prepare. Whichever of the report and the
/// server's deadline sweep comes first, the job on that host either starts on it (the
/// report first) or goes back to the queue with the host barred and runs on the other host
/// once its own job ended (the deadline first, the late report naming an allocation that is
/// no longer live); the winner is not pinned. Both jobs succeed, none is placed on the
/// barred host again within the bar, and every host serves at the end. Red for a server
/// that starts a job on a host while it also places the job elsewhere, runs a job twice,
/// or loses the job in the race.
pub fn a_race(step: i64) -> Scenario {
    let delay = secs(PREPARE_DEADLINE.as_secs().saturating_add_signed(step));
    let name = format!("a_ready_report_racing_the_prepare_deadline_never_double_books_{step}");
    Scenario::new(&name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(RACE_HOSTS))
        .hosts("burst", RACE_HOSTS)
        .work(2, Work::whole_host(RACE_RUN))
        .at(Duration::ZERO, agent_prepare_delay(agents(0..1), delay))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(2, HowEnded::Succeeded))
        .expect(Expect::held_and_released(2))
        .expect(Expect::kept_off_a_failed_host(BACKOFF))
        .expect(Expect::every_host_serving())
        .settle_within(RACE_SETTLE)
}

/// Every scenario of the race, one for each step of [`RACE_SWEEP`].
pub fn race_sweep() -> Vec<Scenario> {
    RACE_SWEEP.map(a_race).collect()
}

/// The restarts of a committed host: three hosts, the first slow to prepare, three jobs.
pub const COMMIT_HOSTS: usize = 3;
const COMMIT_DELAY: Duration = Duration::from_secs(200);
const COMMIT_RUN: Duration = Duration::from_secs(60);
const COMMIT_SETTLE: Duration = Duration::from_secs(900);

/// Proves that an agent that restarts while its host is committed to a job does not
/// double-book it: three hosts, the first agent takes [`COMMIT_DELAY`] to prepare, three
/// whole-host jobs of [`COMMIT_RUN`] at time zero, and the first agent is started again at
/// 20 s, 60 s and 100 s, each time with empty state, saying hello while the job placed on
/// its host is preparing. Each hello restarts the job's prepare clock; the agent is told
/// again and the job starts when it reports ready, and the jobs on the other hosts are not
/// touched: all three succeed, each placed once, and every host serves at the end. Red for
/// a hello that fails the job's prepare or loses the job (a job placed again, or not
/// succeeding), books the restarted host to a second job while the first prepares
/// (`whole-host-is-exclusive`, `no-double-booking`), or leaves an allocation booked.
pub fn restarts_while_committed() -> Scenario {
    Scenario::new("an_agent_that_restarts_while_the_host_is_committed_does_not_double_book")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(COMMIT_HOSTS))
        .hosts("burst", COMMIT_HOSTS)
        .work(COMMIT_HOSTS as u32, Work::whole_host(COMMIT_RUN))
        .at(
            Duration::ZERO,
            agent_prepare_delay(agents(0..1), COMMIT_DELAY),
        )
        .at(secs(20), replace_agents(agents(0..1)))
        .at(secs(60), replace_agents(agents(0..1)))
        .at(secs(100), replace_agents(agents(0..1)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(COMMIT_HOSTS, HowEnded::Succeeded))
        .expect(Expect::jobs_placed_again(0))
        .expect(Expect::no_allocation_live())
        .expect(Expect::every_host_serving())
        .settle_within(COMMIT_SETTLE)
}

/// The restart of a preparing host: one host, slow to prepare, one job.
const RESTART_DELAY: Duration = Duration::from_secs(400);
const RESTART_AT: Duration = Duration::from_secs(300);
const RESTART_SETTLE: Duration = Duration::from_secs(1_200);

/// Proves that an agent's hello restarts the prepare clock of the job placed on its host
/// (`job_whole.rs`, rule 2): one host whose agent takes [`RESTART_DELAY`] to prepare, one
/// whole-host job at time zero, and the agent started again at [`RESTART_AT`], when the
/// server's deadline, [`PREPARE_DEADLINE`] from the placement, is 300 s away and the
/// agent, told again, would report ready only after it. The deadline moves to
/// [`PREPARE_DEADLINE`] after the hello, so the job is still placed on its first
/// allocation when the agent reports ready, starts, runs and succeeds: one allocation, one
/// launch, after the restart. Red for a hello that leaves the placement's deadline where it
/// was (the job fails its prepare at 600 s and is placed a second time), or one that fails
/// the job.
pub fn restart_moves_the_deadline() -> Scenario {
    let job = ItemId(0);
    Scenario::new("an_agent_restart_moves_the_prepare_deadline_of_its_job")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::whole_host(RACE_RUN))
        .at(
            Duration::ZERO,
            agent_prepare_delay(agents(0..1), RESTART_DELAY),
        )
        .at(RESTART_AT, replace_agents(agents(0..1)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::allocations_of(job, 1))
        .expect(Expect::launched_on(&[0], RESTART_AT))
        .expect(Expect::every_host_serving())
        .settle_within(RESTART_SETTLE)
}

/// The cancels: one host, one job.
const CANCEL_RUN: Duration = Duration::from_secs(60);
const CANCEL_DELAY: Duration = Duration::from_secs(60);
const CANCEL_LOOK: Duration = Duration::from_secs(120);
const CANCEL_SETTLE: Duration = Duration::from_secs(1_200);

/// Proves that a whole-host job cancelled while its host prepares is cancelled without
/// waiting for the host, and starts nothing (`job_whole.rs`, rule 1): one host whose agent
/// takes [`CANCEL_DELAY`] to prepare, one whole-host job at time zero, and its cancel
/// asked as soon as a look finds the job placed (`cancel_when_preparing`). The server stops
/// naming the allocation to the agent, which leaves `preparing`, and writes the cancel at
/// the heartbeat after: the job is cancelled and its allocation freed at 50 s, before the
/// agent's delay would have ended, with no container launched, and the host serves again.
/// Red for a server that starts the job after the cancel (a launch, the job succeeding),
/// holds the cancel until the agent reports ready (the job not cancelled at 50 s), or
/// leaves the allocation booked.
pub fn a_cancel_while_the_host_prepares() -> Scenario {
    let job = ItemId(0);
    Scenario::new("a_job_cancelled_while_its_host_prepares_is_cancelled_and_starts_nothing")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::whole_host(CANCEL_RUN))
        .at(
            Duration::ZERO,
            agent_prepare_delay(agents(0..1), CANCEL_DELAY),
        )
        .at(Duration::ZERO, cancel_when_preparing(job, CANCEL_LOOK))
        .expect_at(secs(50), Expect::job_state(job, JobState::Cancelled))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Cancelled))
        .expect(Expect::allocation_step(job, AllocationStep::Freed))
        .expect(Expect::no_job_launched())
        .expect(Expect::every_host_serving())
        .settle_within(CANCEL_SETTLE)
}

/// Proves that a cancel while the host is silent does not wait for the deadline: one host
/// whose agent never answers its prepare, one whole-host job at time zero, cancelled as
/// soon as a look finds it placed. The server stops naming the allocation to the agent,
/// which leaves `preparing` and restores, and the cancel is written at the heartbeat after:
/// the job is cancelled and its allocation freed at 500 s, a hundred seconds before the
/// deadline would have failed its prepare. No container is launched, and the job is not
/// tried a second time. Red for a server that holds the cancel until the deadline (the job
/// still placed at 500 s), puts the cancelled job back in the queue (a second allocation),
/// or never ends it.
pub fn a_cancel_while_the_host_is_silent() -> Scenario {
    let job = ItemId(0);
    let before_the_deadline = PREPARE_DEADLINE - secs(100);
    Scenario::new("a_job_cancelled_while_its_host_is_silent_is_cancelled_before_the_deadline")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::whole_host(CANCEL_RUN))
        .at(Duration::ZERO, agent_silent_on_prepare(agents(0..1)))
        .at(Duration::ZERO, cancel_when_preparing(job, CANCEL_LOOK))
        .expect_at(
            before_the_deadline,
            Expect::job_state(job, JobState::Cancelled),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Cancelled))
        .expect(Expect::allocations_of(job, 1))
        .expect(Expect::allocation_step(job, AllocationStep::Freed))
        .expect(Expect::no_job_launched())
        .expect(Expect::every_host_serving())
        .settle_within(CANCEL_SETTLE)
}
