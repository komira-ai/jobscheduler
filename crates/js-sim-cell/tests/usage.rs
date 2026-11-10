//! The usage history family: the server writes one row of its usage history for each
//! allocation it frees, and the cell reads those rows through the job service's usage route
//! once a scenario has settled. A clean scenario (ordinary and whole-host jobs, cancels, a
//! restart of the server) holds `usage-row-per-ended-allocation` and
//! `usage-totals-match-terminal-jobs`, and the same scenario with a store defect applied to
//! the rows the cell read (a lost, a doubled and a mis-written row, as `UsageReadFault`
//! models them) fails exactly the invariant that names it.
//!
//! Sizes, in agent requests a seed: six hosts beating every 5 s (the server's default) for
//! the 470 s the scenario runs: about 560 heartbeats a seed, with the hellos besides.

use std::time::Duration;

use js_sim_cell::actions::{
    cancel_work, items, restart_server, set_usage_fault, stop_server, submit_work,
};
use js_sim_cell::usage::PAGE_SIZE;
use js_sim_cell::{
    Expect, HowEnded, Invariant, Scenario, ScenarioFailure, ServerSpec, Shape, UsageReadFault, Work,
};

/// Six hosts that book whole.
const HOSTS: usize = 6;

/// Ten ordinary jobs at the start, two whole-host jobs 1 s later, and ten more ordinary
/// jobs after the restart: twenty-two jobs, two of them cancelled while they run.
const FIRST: u32 = 10;
const WHOLE: u32 = 2;
const LATER: u32 = 10;
const CANCELLED: u32 = 2;
const RUN: Duration = Duration::from_secs(40);
const WHOLE_AT: Duration = Duration::from_secs(1);
const CANCEL_AT: Duration = Duration::from_secs(12);
const RESTART_AT: Duration = Duration::from_secs(50);
const LATER_AT: Duration = Duration::from_secs(70);
const SETTLE: Duration = Duration::from_secs(400);

/// How many seeds the clean scenario runs.
const SEEDS: u64 = 3;

/// The scenario every test of the family runs, named `name`, with `fault` applied to the
/// usage rows the cell reads at the end.
fn scenario(name: &str, fault: UsageReadFault) -> Scenario {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), HOSTS);
    let jobs = (FIRST + WHOLE + LATER) as usize;
    Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", HOSTS)
        .work(FIRST, Work::run_for(RUN))
        .at(WHOLE_AT, submit_work(WHOLE, Work::whole_host(RUN)))
        .at(CANCEL_AT, cancel_work(items(2..2 + CANCELLED)))
        .at(RESTART_AT, restart_server())
        .at(LATER_AT, submit_work(LATER, Work::run_for(RUN)))
        .at(Duration::ZERO, set_usage_fault(fault))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            jobs - CANCELLED as usize,
            HowEnded::Succeeded,
        ))
        .expect(Expect::terminal_count(
            CANCELLED as usize,
            HowEnded::Cancelled,
        ))
        .expect(Expect::no_allocation_live())
        .settle_within(SETTLE)
}

/// Proves that the usage history the server tells matches what it freed: the clean
/// scenario's twenty-two allocations (ordinary and whole-host, two cancelled while they
/// ran, a restart of the server between) each have their one row, the rows add up to what
/// the allocations booked, and the cell read them over several pages. States, beside both
/// invariants, that the cell read exactly one row per job, more than a page of them, and
/// that the two whole-host jobs' rows book no CPU. Red for a server that loses, doubles or
/// mis-writes a row at a free (`usage-row-per-ended-allocation`,
/// `usage-totals-match-terminal-jobs`), for a usage route that cuts a page short or
/// repeats one (the count of rows), and for a whole host booked as a share (the rows that
/// book no CPU).
#[test]
fn the_usage_rows_of_a_clean_scenario_match_every_ended_allocation() {
    let jobs = (FIRST + WHOLE + LATER) as usize;
    scenario("clean_usage", UsageReadFault::None)
        .expect_that("one_row_per_job_over_several_pages", move |cell| {
            let Some(rows) = cell.observations().usage.as_ref() else {
                return Some("wanted the usage history read, got none".to_owned());
            };
            let paged = rows.len() > PAGE_SIZE as usize;
            let whole = rows.iter().filter(|row| row.cpu_millis == 0).count();
            let met = (rows.len() == jobs) & paged & (whole == WHOLE as usize);
            let (count, page) = (rows.len(), PAGE_SIZE);
            (!met).then(|| {
                format!(
                    "wanted {jobs} rows over pages of {page}, {WHOLE} of them booking no CPU, got {count} and {whole}"
                )
            })
        })
        .run_seeds(SEEDS);
}

/// The violation the first seed of the scenario with `fault` fails with: its invariant and
/// its line.
fn failure(fault: UsageReadFault) -> (Invariant, String) {
    let ran = scenario("faulted_usage", fault).try_run_seeds(1);
    match ran {
        Err(ScenarioFailure::Violated(report)) => {
            (report.violation.invariant, report.violation.detail)
        }
        Err(other) => panic!("expected a violation, got {other}"),
        Ok(_) => panic!("expected a violation, every seed passed"),
    }
}

/// Proves that a store that lost a row is seen: the clean scenario with its first row
/// dropped from what the cell read fails `usage-row-per-ended-allocation`, naming the job
/// freed, its host, the time of the free and the count of rows (none), and fails nothing
/// else. Catches a check that reads only the rows that exist.
#[test]
fn a_lost_usage_row_fails_the_one_row_per_ended_allocation_invariant() {
    assert_eq!(
        failure(UsageReadFault::DropRow),
        (
            Invariant::UsageRowPerEndedAllocation,
            "job sim-job-2 freed on host h005 at 25068 has 0 usage rows, not one".to_owned()
        )
    );
}

/// Proves that a store that wrote a row twice is seen: the clean scenario with its first
/// row doubled fails `usage-row-per-ended-allocation`, naming the count of rows (two), and
/// fails nothing else. Catches a check that asks for at least one row.
#[test]
fn a_doubled_usage_row_fails_the_one_row_per_ended_allocation_invariant() {
    assert_eq!(
        failure(UsageReadFault::DoubleRow),
        (
            Invariant::UsageRowPerEndedAllocation,
            "job sim-job-2 freed on host h005 at 25068 has 2 usage rows, not one".to_owned()
        )
    );
}

/// Proves that a store that wrote a row with the wrong booking is seen: the clean scenario
/// with the first row's CPU one thousandth of a core over fails
/// `usage-totals-match-terminal-jobs`, naming both totals of the job, and fails nothing
/// else. Catches a check that pairs rows with allocations and does not compare what they
/// booked.
#[test]
fn a_mis_written_usage_row_fails_the_totals_invariant() {
    assert_eq!(
        failure(UsageReadFault::SkewTotal),
        (
            Invariant::UsageTotalsMatchTerminalJobs,
            "the usage rows of job sim-job-2 add up to 1 allocations, cpu 1001 millis, memory \
             1024 bytes, on [h005], and its allocations freed to 1 allocations, cpu 1000 millis, \
             memory 1024 bytes, on [h005]"
                .to_owned()
        )
    );
}

/// Asserts what each fault does to rows: none leaves them, a drop removes the first, a
/// double repeats the first, a skew adds a thousandth of a core to the first; each leaves
/// an empty read empty. Catches a fault that acts on the wrong row, and one that
/// invents a row when there is none.
#[test]
fn each_usage_fault_acts_on_the_first_row_only() {
    use js_sim_cell::UsageRow;
    let row = |n: u64| UsageRow {
        job: js_jobs::JobId::parse(&format!("sim-job-{n}")).expect("a job id"),
        host: js_types::HostId::new("h000").expect("a host id"),
        ended_at: js_types::sched::FarmTime::from_millis(n),
        cpu_millis: 1_000,
        memory_bytes: 1_024,
    };
    let rows = vec![row(1), row(2)];
    let skewed = UsageRow {
        cpu_millis: 1_001,
        ..row(1)
    };
    assert_eq!(UsageReadFault::None.apply(rows.clone()), rows);
    assert_eq!(UsageReadFault::DropRow.apply(rows.clone()), [row(2)]);
    assert_eq!(
        UsageReadFault::DoubleRow.apply(rows.clone()),
        [row(1), row(2), row(1)]
    );
    assert_eq!(UsageReadFault::SkewTotal.apply(rows), [skewed, row(2)]);
    for fault in [
        UsageReadFault::None,
        UsageReadFault::DropRow,
        UsageReadFault::DoubleRow,
        UsageReadFault::SkewTotal,
    ] {
        assert_eq!(fault.apply(Vec::new()), []);
    }
}

/// Proves that a server that is down when the scenario settles has no usage read, and that
/// no usage invariant then flags the allocations it freed before: two jobs run to their end,
/// the server is stopped and never restarted, and the cell read no usage history
/// (`Observations::usage` is `None`) and fails nothing. Catches a read of a down server as
/// an empty history, which would flag every allocation freed as a lost row.
#[test]
fn a_server_that_is_down_at_the_end_has_no_usage_read_and_no_violation() {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), 2);
    #[rustfmt::skip]
    let scenario = Scenario::new("a_server_that_is_down_at_the_end_has_no_usage_read_and_no_violation");
    scenario
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", 2)
        .work(2, Work::run_for(Duration::from_secs(10)))
        .at(Duration::from_secs(100), stop_server())
        .expect(Expect::all_items_terminal())
        .expect(Expect::no_allocation_live())
        .expect_that("no_usage_read", |cell| {
            let read = cell.observations().usage.as_ref();
            read.map(|rows| format!("wanted no usage read, got {} rows", rows.len()))
        })
        .settle_within(Duration::from_secs(60))
        .run_seeds(1);
}
