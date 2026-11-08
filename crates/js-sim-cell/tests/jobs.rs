//! The job scenario family: the rows of design section 8 that the job design closes and
//! the lease routes could not, each over the job front ([`Scenario::over_jobs`]): a submit
//! whose answer the client lost, sent again under the same job id (row 1); a host silent
//! before it lists a job's start, which the start timeout ends (rows 35 and 43); a job
//! cancelled before its host lists the start, with a late start after the stop (row 38);
//! the headline over jobs (no row); and a restart of the server with jobs in flight (rows
//! 3 and 36). Each runs over sixteen seeds under every invariant of `Invariant::ALL` (the
//! five job invariants among them) and states the rest with [`Expect`]s only.
//!
//! Each states what the design promises and the server as built does. Rows 37 and 42
//! have no scenario here: the cell cannot bring about what each starts from (see
//! `docs/simulation.md`, "Which section 8 rows have a scenario").
//!
//! Sizes, in agent requests a seed (heartbeats, with the hellos besides), each at most
//! 30,000:
//! - the lost answer: [`RESUBMIT_HOSTS`] beating every 5 s for [`RESUBMIT_AT`] plus
//!   [`RESUBMIT_SETTLE`], 310 s, make about 250;
//! - the silent host: one host beating every [`SILENT_INTERVAL`] for [`SILENT_CUT_AT`],
//!   [`SILENT_CUT_FOR`] and [`SILENT_SETTLE`], 1,500 s, makes about 50;
//! - the cancel before the start is listed: [`EARLY_HOSTS`] beating every second for
//!   [`EARLY_QUIET_AT`] plus [`EARLY_SETTLE`], 150 s, make about 1,200;
//! - the headline: about 10,700 (`cell/jobs_headline.rs`);
//! - the restart: [`RESTART_HOSTS`] beating every 5 s for [`RESTART_AT`] plus
//!   [`RESTART_SETTLE`], 1,080 s, less the [`RESTART_AT`] minus [`STOP_AT`] the server is
//!   down, make about 4,100, with a hello from every host after the restart besides.

#[path = "cell/jobs_headline.rs"]
mod jobs_headline;

use std::time::Duration;

use js_jobs::{AllocationStep, JobState, StartFailure};
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{
    agents, cancel_work, items, lose_submit_answer, partition_agents, restart_server,
    set_bus_faults, stop_server, submit_work,
};
use js_sim_cell::{
    Call, CallKind, Counter, Expect, HowEnded, ItemId, JobFront, LostAnswer, Scenario, ServerSpec,
    Shape, Work,
};

/// Every scenario of the family runs over this many seeds.
const SEEDS: u64 = 16;

/// The lost answer: four hosts at the server's default heartbeat; one job submitted at
/// 10 s, once every host has said hello, so its first submit is accepted.
const RESUBMIT_HOSTS: usize = 4;
const RESUBMIT_AT: Duration = Duration::from_secs(10);
const RESUBMIT_RUN: Duration = Duration::from_secs(60);
const RESUBMIT_SETTLE: Duration = Duration::from_secs(300);

/// The silent host: one host beating every 30 s, so a start placed at about 5 s reaches it
/// on the answer to its heartbeat at about 30 s and the next heartbeat, which would list
/// the job, is due at about 60 s. It is cut off at 45 s, between the two, for longer than
/// the server's start timeout (15 minutes after placement, the default) and checked at
/// [`SILENT_CHECK_AT`], still cut off, after the timeout has ended the job. The job runs
/// longer than all of it, so the host still runs it when the cut heals.
const SILENT_INTERVAL: Duration = Duration::from_secs(30);
const SILENT_GRACE: Duration = Duration::from_secs(90);
const SILENT_RUN: Duration = Duration::from_secs(3_000);
const SILENT_CUT_AT: Duration = Duration::from_secs(45);
const SILENT_CUT_FOR: Duration = Duration::from_secs(1_155);
const SILENT_CHECK_AT: Duration = Duration::from_secs(1_100);
const SILENT_SETTLE: Duration = Duration::from_secs(300);

/// The cancel before the start is listed, on the settings of the lease routes' stale
/// start (`tests/faults.rs`): hosts beat every second; every job is cancelled while its
/// start is on the way, on a bus that delays a message by more than the interval and lets
/// a later one overtake it, so a start can arrive after the stop that follows it. The
/// grace of 6 s keeps a delayed host from being held lost.
const EARLY_HOSTS: usize = 8;
const EARLY_INTERVAL: Duration = Duration::from_secs(1);
const EARLY_GRACE: Duration = Duration::from_secs(6);
const EARLY_JOBS: u32 = 16;
const EARLY_RUN: Duration = Duration::from_secs(30);
const EARLY_GIVE_UP: Duration = Duration::from_secs(300);
const EARLY_FAULTS_AT: Duration = Duration::from_secs(2);
const EARLY_MIN_DELAY: Duration = Duration::from_millis(1_000);
const EARLY_MAX_DELAY: Duration = Duration::from_millis(4_000);
const EARLY_REORDER_PERCENT: u32 = 100;
const EARLY_CANCEL_AT: Duration = Duration::from_millis(6_500);
const EARLY_QUIET_AT: Duration = Duration::from_secs(30);
const EARLY_SETTLE: Duration = Duration::from_secs(120);

/// The headline's seeds run on this many threads, a quarter each: it is the family's
/// largest by far.
const HEADLINE_THREADS: u64 = 4;

/// The restart: twenty hosts at the server's default heartbeat, room for 160 jobs at once,
/// and 180 jobs of 300 s, so twenty wait queued; the server is stopped at 120 s, while the
/// first 160 run, for 60 s (longer than the 15 s grace), and started again over the same
/// directory and store.
const RESTART_HOSTS: usize = 20;
const RESTART_JOBS: u32 = 180;
const RESTART_RUN: Duration = Duration::from_secs(300);
const RESTART_GIVE_UP: Duration = Duration::from_secs(2_400);
const STOP_AT: Duration = Duration::from_secs(120);
const RESTART_AT: Duration = Duration::from_secs(180);
/// When, after the restart, every job is checked to be still queued, placed or running.
const RESTART_CHECK_AT: Duration = Duration::from_secs(240);
const RESTART_SETTLE: Duration = Duration::from_secs(900);

/// Covers section 8, row 1 (submit answer lost: the job id closes it, section 5.3). Four
/// hosts; once they have said hello, the client submits one job and loses the answer to
/// the submit (the job front's own fault, thrown by `lose_submit_answer`: it reads the
/// answer as none), so it submits again under the same job id at its next poll. States
/// that both submits were answered 201 (the first answer was a job made, and lost), that
/// the server's table held exactly one job and the job one allocation, that it was
/// launched once and succeeded. Red for a server that makes a second job, or a second
/// placement, for a repeated submit (`jobs-read`, `allocations`, `each-job-launched-once`,
/// `one-allocation-per-job`), and one that refuses the repeat (`answer`, the count of
/// successes).
#[test]
fn a_resubmitted_job_id_after_a_lost_answer_is_one_job() {
    let lost = LostAnswer::new();
    let submit = |nth| Call::new(ItemId(0), CallKind::Queue, nth);
    Scenario::new("a_resubmitted_job_id_after_a_lost_answer_is_one_job")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            RESUBMIT_HOSTS,
        ))
        .hosts("burst", RESUBMIT_HOSTS)
        .over_jobs()
        .front(JobFront::losing_answers(&lost))
        .at(RESUBMIT_AT, lose_submit_answer(&lost))
        .at(RESUBMIT_AT, submit_work(1, Work::run_for(RESUBMIT_RUN)))
        .expect(Expect::answer(submit(1), 201, None))
        .expect(Expect::answer(submit(2), 201, None))
        .expect(Expect::jobs_read(1))
        .expect(Expect::allocations_of(ItemId(0), 1))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .settle_within(RESUBMIT_SETTLE)
        .run_seeds(SEEDS);
}

/// Covers section 8, rows 35 (host silent while a job is placed or starting: never
/// requeued on silence; the start timeout ends it and its allocation stays releasing
/// until the host confirms) and 43 (a start never completes: failed to start,
/// `start-timeout`; the allocation freed once the host confirms, kept while it is
/// silent). One host is placed a job and cut off after the start reached it and before
/// any heartbeat listed the job, for longer than the start timeout. States, while it is
/// still cut off, that the job was last read failed to start with `start-timeout` and its
/// allocation releasing; and at the end, after the host came back still running the job,
/// was told to stop it and confirmed it gone, that the allocation was freed, that the job
/// is still `start-timeout`, had one allocation and was launched once, and that the
/// client heard it failed. The witness shows the start had reached the host before the
/// cut in some seed: it ran a job no heartbeat of its own had listed. Red for a server
/// that requeues a placed job on silence (`allocations`, `job-state`), frees the
/// allocation of a silent host (`allocation-step` while cut off,
/// `freed-only-when-confirmed`), never ends the job (`job-state`), or never frees the
/// allocation once the host confirms (`allocation-step` at the end).
#[test]
fn a_silent_host_never_requeues_a_placed_job_and_start_timeout_ends_it() {
    let timeout = JobState::FailedToStart(StartFailure::StartTimeout);
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), 1)
        .heartbeat(SILENT_INTERVAL, SILENT_GRACE);
    Scenario::new("a_silent_host_never_requeues_a_placed_job_and_start_timeout_ends_it")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", 1)
        .work(1, Work::run_for(SILENT_RUN))
        .at(
            SILENT_CUT_AT,
            partition_agents(agents(0..1), SILENT_CUT_FOR),
        )
        .expect_at(SILENT_CHECK_AT, Expect::job_state(ItemId(0), timeout))
        .expect_at(
            SILENT_CHECK_AT,
            Expect::allocation_step(ItemId(0), AllocationStep::Releasing),
        )
        .expect(Expect::job_state(ItemId(0), timeout))
        .expect(Expect::allocation_step(ItemId(0), AllocationStep::Freed))
        .expect(Expect::allocations_of(ItemId(0), 1))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::terminal_count(1, HowEnded::Failed))
        .witness("cut_with_unlisted_run", |c| {
            c.get(Counter::CutWithUnlistedRun) >= 1
        })
        .settle_within(SILENT_SETTLE)
        .over_jobs()
        .run_seeds(SEEDS);
}

/// Covers section 8, row 38 (cancel of a placed job whose start the host never listed:
/// the start is no longer sent, the agent retires the id on the stop, the job ends
/// cancelled and its allocation is freed; a late start does not run). Every job is
/// cancelled while its start is still on the way, on a bus that delays a message past
/// the 1 s heartbeat interval and lets a later message overtake an earlier one (a
/// request retried over a second connection), so a start can reach its agent after the
/// stop that follows it. States that every job ends cancelled, for the client and in the
/// server's table, that no allocation is left booked or releasing, and that no job is
/// launched twice. The witness shows a start arrived after its stop in some seed. Red
/// for a server that keeps sending a start after the cancel is committed, or leaves the
/// allocation booked (`no-allocation-live`), and for one that ends such a job otherwise
/// than cancelled (`jobs-in`, the count of cancels). That the allocation is freed in the
/// same commit as the end is not stated: a job whose start was listed before the cancel
/// ends the same way, its allocation freed a heartbeat later.
#[test]
fn a_cancel_before_the_host_lists_the_start_ends_cancelled_and_frees_the_allocation() {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), EARLY_HOSTS)
        .heartbeat(EARLY_INTERVAL, EARLY_GRACE);
    let late = Faults {
        min_delay: EARLY_MIN_DELAY,
        max_delay: EARLY_MAX_DELAY,
        reorder: Chance::percent(EARLY_REORDER_PERCENT),
        ..Faults::default()
    };
    let jobs = EARLY_JOBS as usize;
    // The page's check (`tests/docs.rs`) finds a scenario by `Scenario::new("<name>")` on
    // one line, which this name makes longer than rustfmt's width.
    #[rustfmt::skip]
    let scenario = Scenario::new("a_cancel_before_the_host_lists_the_start_ends_cancelled_and_frees_the_allocation");
    scenario
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", EARLY_HOSTS)
        .work(
            EARLY_JOBS,
            Work::run_for(EARLY_RUN).give_up_after(EARLY_GIVE_UP),
        )
        .at(EARLY_FAULTS_AT, set_bus_faults(late))
        .at(EARLY_CANCEL_AT, cancel_work(items(0..EARLY_JOBS)))
        .at(EARLY_QUIET_AT, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(jobs, HowEnded::Cancelled))
        .expect(Expect::jobs_in_state(jobs, JobState::Cancelled))
        .expect(Expect::no_allocation_live())
        .expect(Expect::each_job_launched_once())
        .witness("start_after_stop", |c| c.get(Counter::StartAfterStop) >= 1)
        .settle_within(EARLY_SETTLE)
        .over_jobs()
        .run_seeds(SEEDS);
}

/// The headline over jobs (no row of section 8: it gathers sections 6.2, 6.3 and 7):
/// two hundred hosts and four hundred jobs, twenty agents cut off for 30 s, and ten
/// agents killed mid-job and replaced; the scenario and what it states are in
/// `cell/jobs_headline.rs`. Every job reaches one terminal state, nothing is
/// double-booked, and no allocation is booked once every job is terminal. Red for a
/// server that books a job twice or past a host's capacity (`one-allocation-per-job`,
/// `no-double-booking`), runs a job again (`each-job-launched-once`), frees an
/// allocation on silence (`freed-only-when-confirmed`), never ends a killed host's jobs
/// (`jobs-ended`, `all-items-terminal`), or leaves their allocations booked
/// (`no-allocation-live`). Its seeds run on [`HEADLINE_THREADS`] threads, each building
/// the scenario for its quarter.
#[test]
fn the_headline_over_jobs() {
    let parts: Vec<_> = (0..HEADLINE_THREADS)
        .map(|part| {
            let seeds = part * SEEDS / HEADLINE_THREADS..(part + 1) * SEEDS / HEADLINE_THREADS;
            std::thread::spawn(move || {
                let ran = jobs_headline::the_headline_over_jobs().try_run_seed_range(seeds);
                ran.map(|_| ()).map_err(|failure| failure.to_string())
            })
        })
        .collect();
    for part in parts {
        match part.join() {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => panic!("{failure}"),
            Err(panicked) => std::panic::resume_unwind(panicked),
        }
    }
}

/// Covers section 8, rows 3 (a restart with a job in flight: the host keeps it and lists
/// it, and the client reads the job) and 36 (a restart ends no running job and a queued
/// job keeps waiting; the outage here is a minute, not longer than a queue wait). Twenty
/// hosts run 160 jobs and twenty more wait queued when the server is stopped for 60 s and
/// started again over its own directory and store (`--boot resume`); the hosts are told
/// they are unknown, say hello again and list what they hold. States that after the
/// restart every job is still in the server's table and none has ended
/// (`jobs-read`, `jobs-ended` at [`RESTART_CHECK_AT`]); and at the end that every job
/// succeeded, for the client and in the server's table, that no job was launched twice
/// under any allocation, and that no allocation is left booked or releasing. The witness
/// shows the restart reached the hosts: a heartbeat was answered `unknown-host`. Red for
/// a restarted server that forgets a job or ends it (`jobs-read`, `jobs-ended`, the
/// counts of successes), loses a running job its host lists again (`jobs-in`), places a
/// job again (`each-job-launched-once`, `one-allocation-per-job`), or leaves an
/// allocation behind (`no-allocation-live`).
#[test]
fn a_server_restart_with_jobs_in_flight_loses_no_job() {
    let jobs = RESTART_JOBS as usize;
    let work = Work::run_for(RESTART_RUN).give_up_after(RESTART_GIVE_UP);
    Scenario::new("a_server_restart_with_jobs_in_flight_loses_no_job")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            RESTART_HOSTS,
        ))
        .hosts("burst", RESTART_HOSTS)
        .work(RESTART_JOBS, work)
        .at(STOP_AT, stop_server())
        .at(RESTART_AT, restart_server())
        .expect_at(RESTART_CHECK_AT, Expect::jobs_read(jobs))
        .expect_at(RESTART_CHECK_AT, Expect::jobs_ended(0))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(jobs, HowEnded::Succeeded))
        .expect(Expect::jobs_in_state(jobs, JobState::Succeeded))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::no_allocation_live())
        .witness("unknown_host", |c| c.get(Counter::UnknownHost) >= 1)
        .settle_within(RESTART_SETTLE)
        .over_jobs()
        .run_seeds(SEEDS);
}
