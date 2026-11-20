//! The job scenario family: the rows of design section 8 that the job design closes and
//! earlier scenarios did not reach, each over the job front: a submit
//! whose answer the client lost, sent again under the same job id (row 1); a host silent
//! before it lists a job's start, which the start timeout ends (rows 35 and 43); a job
//! cancelled before its host lists the start (row 38);
//! the headline over jobs (no row); a restart of the server with jobs in flight (rows
//! 3 and 36); and a start that a host held for maintenance refused between its hello and its
//! cordon, which sends the job to another host with no failure (no row). Each runs over sixteen seeds under every invariant of `Invariant::ALL` (the
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
//! - the headline: about 670 (`cell/jobs_headline.rs`);
//! - the restart: [`RESTART_HOSTS`] beating every 5 s for [`RESTART_AT`] plus
//!   [`RESTART_SETTLE`], 490 s, less the [`RESTART_AT`] minus [`STOP_AT`] the server is
//!   down, make about 370, with no hello after the restart (the sessions are rows in the store);
//! - the held host: [`HELD_HOSTS`] real agents beating every 5 s for [`HELD_UNCORDON_AT`]
//!   plus [`HELD_SETTLE`], 360 s, make about 150.
//!
//! The headline and the restart are smaller than a farm of hosts: every scenario here runs
//! in the pull request's `test` and `coverage` jobs, which a slow scenario must not hold
//! up, and sixteen seeds of two hundred hosts and four hundred jobs take minutes each.

#[path = "cell/jobs_headline.rs"]
mod jobs_headline;

use std::time::Duration;

use js_jobs::{AllocationStep, JobState, StartFailure};
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{
    agents, cancel_work, cordon_host, hold_agents, items, lose_submit_answer, partition_agents,
    restart_server, set_bus_faults, stop_server, submit_work, uncordon_host,
};
use js_sim_cell::{
    Call, CallKind, Cell, Counter, Expect, HowEnded, ItemId, JobFront, LostAnswer, Route, Scenario,
    ServerSpec, Shape, Work,
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

/// The cancel before the start is listed: hosts beat every second; every job is cancelled
/// while its start is on the way, on a bus that delays a message by more than the interval
/// and lets a later one overtake it, so the cancel races the start. The grace of 6 s keeps
/// a delayed host from being held lost.
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

/// The restart: four hosts at the server's default heartbeat, room for 32 jobs at once,
/// and 36 jobs of 150 s, so four wait queued; the server is stopped at 60 s, while the
/// first 32 run, for 30 s (longer than the 15 s grace), and started again over the same
/// directory and store.
const RESTART_HOSTS: usize = 4;
const RESTART_JOBS: u32 = 36;
const RESTART_RUN: Duration = Duration::from_secs(150);
const RESTART_GIVE_UP: Duration = Duration::from_secs(2_400);
const STOP_AT: Duration = Duration::from_secs(60);
const RESTART_AT: Duration = Duration::from_secs(90);
/// When, after the restart, every job is checked to be still queued, placed or running.
const RESTART_CHECK_AT: Duration = Duration::from_secs(120);
const RESTART_SETTLE: Duration = Duration::from_secs(400);

/// The held host: two hosts of the real agent, which say hello at the start. The operator
/// cordons `h001` at [`HELD_OTHER_CORDON_AT`] for an hour, so the job can go only to `h000`;
/// `h000` is held at [`HELD_AT`] (its hold file written, as an update's drain writes it) and
/// the job submitted at [`HELD_SUBMIT_AT`], so it is placed on `h000` before `h000` is
/// cordoned, at [`HELD_CORDON_AT`] (the cordon a held agent's keeper places, for an hour);
/// `h001` is uncordoned at [`HELD_UNCORDON_AT`]. Between the refusal and the cordon of `h000`
/// several placement passes run, with `h000` open and serving.
const HELD_HOSTS: usize = 2;
const HELD_OTHER_CORDON_AT: Duration = Duration::from_secs(5);
const HELD_AT: Duration = Duration::from_secs(8);
const HELD_SUBMIT_AT: Duration = Duration::from_secs(10);
const HELD_CORDON_AT: Duration = Duration::from_secs(40);
const HELD_UNCORDON_AT: Duration = Duration::from_secs(60);
const HELD_OTHER_OPEN_AT: Duration = Duration::from_secs(12);
const HELD_RUN: Duration = Duration::from_secs(60);
const HELD_SETTLE: Duration = Duration::from_secs(300);
const HOUR_SECS: u32 = 3_600;

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
        .run_seeds(SEEDS);
}

/// Covers section 8, row 38 (cancel of a placed job whose start the host never listed:
/// the start is no longer sent, the agent retires the id on the stop, the job ends
/// cancelled and its allocation is freed). Every job is cancelled while its start is
/// still on the way, on a bus that delays a message past the 1 s heartbeat interval and
/// lets a later message overtake an earlier one (a request retried over a second
/// connection), so the cancel races the start. States that every job ends cancelled, for
/// the client and in the server's table, that no allocation is left booked or releasing,
/// and that no job is launched twice. The witness shows the race happened: in some seed
/// an agent is sent the stop of a job that no answer it read carried the start of, and
/// on this bus, which delays an answer by less than the agent's 10 s limit and drops
/// none, that means the server committed the cancel before it sent the start. A start after its stop is not reachable here: the
/// agent keeps one heartbeat in flight, so answers reach it in the order the server built
/// them, and the server sends no start for a job with a cancel committed; the agent's own
/// fence against one is covered by `per_job_tests.rs` and `real_jobs_tests.rs`. Red
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
        .witness("stop_before_start", |c| {
            c.get(Counter::StopBeforeStart) >= 1
        })
        .settle_within(EARLY_SETTLE)
        .run_seeds(SEEDS);
}

/// The headline over jobs (no row of section 8: it gathers sections 6.2, 6.3 and 7):
/// twenty hosts and forty jobs, four agents cut off for 30 s, and two
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

/// What a restart over the store must not cost an agent: a host's session is its row in the
/// store, so the restarted server serves the old session's beats. No heartbeat of any agent
/// was answered `unknown-host`, and no agent said hello more than the once it did at the
/// start.
fn sessions_survive_the_restart(cell: &mut Cell) -> Option<String> {
    let agents = cell.agent_count();
    let unknown: u64 = (0..agents)
        .filter_map(|index| cell.counters(index))
        .map(|counters| counters.get(Counter::UnknownHost))
        .sum();
    let hellos = cell
        .server()
        .map_or(0, |server| server.answered_total(Route::Hello));
    (unknown > 0 || hellos > agents as u64)
        .then(|| format!("{unknown} unknown-host answers and {hellos} hellos from {agents} agents"))
}

/// Covers section 8, rows 3 (a restart with a job in flight: the host keeps it and lists
/// it, and the client reads the job) and 36 (a restart ends no running job and a queued
/// job keeps waiting; the outage here is 30 s, not longer than a queue wait). Four hosts
/// run 32 jobs and four more wait queued when the server is stopped for 30 s and
/// started again over its own directory and store (`--boot resume`); the hosts keep their
/// sessions (the server serves them from the host rows) and list what they hold. States that after the
/// restart every job is still in the server's table and none has ended
/// (`jobs-read`, `jobs-ended` at [`RESTART_CHECK_AT`]); and at the end that every job
/// succeeded, for the client and in the server's table, that no job was launched twice
/// under any allocation, and that no allocation is left booked or releasing. A statement
/// shows no host lost its session: no heartbeat was answered `unknown-host` and no host said
/// hello again. Red for
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
        .expect_that(
            "no host lost its session to the restart",
            sessions_survive_the_restart,
        )
        .settle_within(RESTART_SETTLE)
        .run_seeds(SEEDS);
}

/// The held host's scenario named `name`, `h001` uncordoned at `uncordon_at` (see
/// [`HELD_HOSTS`]): the job succeeds, read under two allocations, the first on `h000` and the
/// last on `h001`, launched once and only on `h001`, never read failed to start, with no host
/// tainted at the end.
fn held_host(name: &str, uncordon_at: Duration) -> Scenario {
    let scratch = format!("{}/held-host", env!("CARGO_TARGET_TMPDIR"));
    Scenario::new(name)
        .scratch(&scratch)
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            HELD_HOSTS,
        ))
        .real_hosts("burst", HELD_HOSTS)
        .at(HELD_OTHER_CORDON_AT, cordon_host(1, HOUR_SECS))
        .at(HELD_AT, hold_agents(agents(0..1)))
        .at(HELD_SUBMIT_AT, submit_work(1, Work::run_for(HELD_RUN)))
        .at(uncordon_at, uncordon_host(1))
        .at(HELD_CORDON_AT, cordon_host(0, HOUR_SECS))
        .expect(Expect::job_state(ItemId(0), JobState::Succeeded))
        .expect(Expect::allocations_of(ItemId(0), 2))
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::launched_on(&[1], uncordon_at))
        .expect(Expect::no_allocation_live())
        .expect_that("first-on-the-held-host-last-on-the-other", |cell| {
            let reads = &cell.observations().allocation_reads;
            let hosts: Vec<String> = reads.iter().map(|read| read.row.host.to_string()).collect();
            let first = hosts.first().map(String::as_str);
            let last = hosts.last().map(String::as_str);
            (first != Some("h000") || last != Some("h001"))
                .then(|| format!("the job's allocations were read on {hosts:?}"))
        })
        .expect_that("never-failed-to-start", |cell| {
            let reads = &cell.observations().job_reads;
            let failed = reads
                .iter()
                .find(|read| matches!(read.state, JobState::FailedToStart(_)));
            failed.map(|read| {
                format!(
                    "job {} was read {:?} at {:?}",
                    read.job, read.state, read.at
                )
            })
        })
        .settle_within(HELD_SETTLE)
}

/// A host held between its hello and its cordon, with no other host open yet: two hosts of
/// the real agent, `h001` cordoned by the operator; `h000` is held (its agent refuses every
/// start `host-held`) and a job is submitted, which is placed on `h000`, its only open host;
/// `h000` is cordoned 30 s later, as a held agent cordons itself, and `h001` uncordoned after
/// that, so the job waits queued in between. States that the job succeeded, read under two
/// allocations, the first on `h000` and the last on `h001`, launched once (only on `h001`),
/// and was never read failed to start. Red for a server that fails the job on the refusal
/// (`job-state`, `never-failed-to-start`), one that places it on `h000` again while `h000` is
/// held and not yet cordoned (the next refusals are freed without a result the cell writes
/// down, and would use up the job's requeues), and one that frees the refused allocation
/// without the host's word (`freed-only-when-confirmed`).
#[test]
fn a_job_a_held_host_refused_before_its_cordon_waits_and_ends_on_another_host() {
    held_host(
        "a_job_a_held_host_refused_before_its_cordon_waits_and_ends_on_another_host",
        HELD_UNCORDON_AT,
    )
    .run_seeds(SEEDS);
}

/// As the scenario above, with `h001` uncordoned at [`HELD_OTHER_OPEN_AT`], after the job was
/// placed on `h000` and before `h000`'s refusal reaches the server: the requeue and the
/// placement on `h001` happen in one heartbeat, so the cell reads the refused allocation's row
/// taken by the new placement. States the same. Red as above, and for a check that takes the
/// row a new placement took for a free on silence.
#[test]
fn a_job_a_held_host_refused_before_its_cordon_goes_to_an_open_host_at_once() {
    held_host(
        "a_job_a_held_host_refused_before_its_cordon_goes_to_an_open_host_at_once",
        HELD_OTHER_OPEN_AT,
    )
    .run_seeds(SEEDS);
}
