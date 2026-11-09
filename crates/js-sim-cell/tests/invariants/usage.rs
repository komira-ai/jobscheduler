//! The usage invariants' negative cases (in [`NEGATIVE_CASES`], which the parent's table
//! check reads with its own) and their correct siblings, and the boundaries of what each
//! reads: `usage-row-per-ended-allocation` and `usage-totals-match-terminal-jobs`.
//!
//! Both are end checks over the usage rows the cell read at the end (`Observations::usage`)
//! and the allocation rows it read over the run, so every case is at the end of a settled
//! run (`END`), and a case that has no usage read (`None`) is a run in which the cell read
//! none.

use js_jobs::{AllocationId, AllocationStep, Booked, JobId};
use js_sim_cell::{AllocationRow, UsageRow};

use super::*;

/// Every negative case of a usage invariant and the one invariant it must trip, as the
/// parent's `NEGATIVE_CASES`.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::UsageRowPerEndedAllocation, lost_row),
    (Invariant::UsageRowPerEndedAllocation, doubled_row),
    (
        Invariant::UsageRowPerEndedAllocation,
        row_for_a_booked_allocation,
    ),
    (Invariant::UsageTotalsMatchTerminalJobs, skewed_cpu),
    (
        Invariant::UsageTotalsMatchTerminalJobs,
        skewed_second_allocation,
    ),
];

pub(crate) fn job(n: u64) -> JobId {
    JobId::parse(&format!("sim-job-{n}")).expect("a job id")
}

fn allocation(n: u8) -> AllocationId {
    AllocationId::from_bits([n; 16])
}

/// What an ordinary allocation books: a core and a MiB.
fn share() -> Booked {
    Booked::Shape {
        cpu_millis: 1_000,
        memory_bytes: 1 << 20,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// Allocation `a` of job `j` on host `h`, booking `booked`, created at `created`, read at
/// `step`, freed at `freed` when it is.
fn alloc(
    j: u64,
    a: u8,
    h: u64,
    (created, freed): (u64, Option<u64>),
    step: AllocationStep,
    booked: Booked,
) -> AllocationRow {
    AllocationRow {
        job: job(j),
        allocation: allocation(a),
        host: host(h),
        step,
        created_at: ms(created),
        end_reason: None,
        freed_at: freed.map(ms),
        booked,
    }
}

/// An ordinary allocation `a` of job `j` on host `h`, created at `created` and freed at
/// `freed`.
fn freed(j: u64, a: u8, h: u64, created: u64, freed: u64) -> AllocationRow {
    let when = (created, Some(freed));
    alloc(j, a, h, when, AllocationStep::Freed, share())
}

/// The row of the usage history for job `j` on host `h`, ended at `ended`, booking `cpu`
/// and a MiB.
fn row(j: u64, h: u64, ended: u64, cpu: u64) -> UsageRow {
    UsageRow {
        job: job(j),
        host: host(h),
        ended_at: ms(ended),
        cpu_millis: cpu,
        memory_bytes: 1 << 20,
    }
}

/// A settled run that read `allocations`, in order, at 5000, and the usage `rows` at the
/// end.
fn at_end(allocations: Vec<AllocationRow>, rows: Vec<UsageRow>) -> Observations {
    let mut observations = Observations::quiet().at(ms(END)).with_usage(rows);
    for row in allocations {
        observations = observations.with_allocation_read(row, ms(END));
    }
    observations
}

// ---- The negative cases. ----

/// Job 0 was freed on host 0 at 900 and the usage history has no row of it.
pub fn lost_row() -> Observations {
    at_end(vec![freed(0, 1, 0, 100, 900)], vec![])
}

/// Job 0 was freed on host 0 at 900 and the usage history holds its row twice.
pub fn doubled_row() -> Observations {
    let once = row(0, 0, 900, 1_000);
    at_end(vec![freed(0, 1, 0, 100, 900)], vec![once.clone(), once])
}

/// Job 0's allocation is still booked, created at 100, and the usage history has a row of
/// job 0 ended at 900.
pub fn row_for_a_booked_allocation() -> Observations {
    let booked = alloc(0, 1, 0, (100, None), AllocationStep::Booked, share());
    at_end(vec![booked], vec![row(0, 0, 900, 1_000)])
}

/// Job 0 was freed on host 0 at 900 having booked 1000 millis of CPU, and its row says
/// 1001.
pub fn skewed_cpu() -> Observations {
    at_end(vec![freed(0, 1, 0, 100, 900)], vec![row(0, 0, 900, 1_001)])
}

/// Job 0 was freed on host 0 at 300 and on host 1 at 900, each booking 1000 millis; its
/// rows are as many, and the second says 2000.
pub fn skewed_second_allocation() -> Observations {
    at_end(
        vec![freed(0, 1, 0, 100, 300), freed(0, 2, 1, 300, 900)],
        vec![row(0, 0, 300, 1_000), row(0, 1, 900, 2_000)],
    )
}

// ---- The tests. ----

/// Asserts that an ended allocation with no row is flagged by `UsageRowPerEndedAllocation`
/// and by nothing else, and that its row, once read, clears it. Catches a check that
/// counts only rows that exist (a lost row passes), or that flags the totals of a job whose
/// row is lost too.
#[test]
fn an_ended_allocation_with_no_row_is_flagged() {
    assert_eq!(
        lines(&verdict(Invariant::UsageRowPerEndedAllocation, &lost_row())),
        [
            "usage-row-per-ended-allocation at 5000: job sim-job-0 freed on host host-0 at 900 \
             has 0 usage rows, not one"
        ]
    );
    let read = at_end(vec![freed(0, 1, 0, 100, 900)], vec![row(0, 0, 900, 1_000)]);
    none(&verdict(Invariant::UsageRowPerEndedAllocation, &read));
    none(&end(&read));
}

/// Asserts that a row written twice is flagged, with the count, and that the totals of its
/// job are not also flagged. Catches a check that accepts at least one row.
#[test]
fn a_row_written_twice_is_flagged() {
    assert_eq!(
        lines(&end(&doubled_row())),
        [
            "usage-row-per-ended-allocation at 5000: job sim-job-0 freed on host host-0 at 900 \
             has 2 usage rows, not one"
        ]
    );
}

/// Asserts that a row for an allocation still booked is flagged, naming the job, the host
/// and the end; and that the same row is not flagged once the allocation is read freed at
/// that time. Catches a check that does not read rows against the allocations (a row for
/// a live allocation passes).
#[test]
fn a_row_for_an_allocation_that_has_not_ended_is_flagged() {
    assert_eq!(
        lines(&end(&row_for_a_booked_allocation())),
        [
            "usage-row-per-ended-allocation at 5000: a usage row says job sim-job-0 ended on \
             host host-0 at 900, and no allocation of it was read freed then"
        ]
    );
    let releasing = alloc(0, 1, 0, (100, None), AllocationStep::Releasing, share());
    let found = end(&at_end(vec![releasing], vec![row(0, 0, 900, 1_000)]));
    assert_eq!(flagged(&found), [Invariant::UsageRowPerEndedAllocation]);
    let ended = at_end(vec![freed(0, 1, 0, 100, 900)], vec![row(0, 0, 900, 1_000)]);
    none(&end(&ended));
}

/// Asserts that a row for a job the cell read no allocation of is flagged: the cell cannot
/// excuse it as the row of a placement it did not see. Catches an excuse that reads a job
/// with no allocation as one with every.
#[test]
fn a_row_for_a_job_with_no_allocation_read_is_flagged() {
    let found = end(&at_end(vec![], vec![row(7, 0, 900, 1_000)]));
    assert_eq!(
        lines(&found),
        [
            "usage-row-per-ended-allocation at 5000: a usage row says job sim-job-7 ended on \
             host host-0 at 900, and no allocation of it was read freed then"
        ]
    );
}

/// Asserts the boundary of the excuse for a row of a placement the cell never read freed:
/// a job whose latest allocation the cell read was created at 300 has a row ended at 300
/// excused (the earlier placement freed in the same millisecond the next was made) and a
/// row ended at 301 flagged. Catches an excuse a millisecond too wide or too narrow.
#[test]
fn the_row_of_an_unseen_earlier_placement_is_excused_up_to_the_next_creation() {
    let latest = alloc(0, 2, 1, (300, None), AllocationStep::Booked, share());
    let at = |ended| at_end(vec![latest.clone()], vec![row(0, 0, ended, 1_000)]);
    none(&end(&at(300)));
    none(&end(&at(120)));
    assert_eq!(
        flagged(&end(&at(301))),
        [Invariant::UsageRowPerEndedAllocation]
    );
}

/// Asserts that a row whose CPU differs from what the allocation booked is flagged by
/// `UsageTotalsMatchTerminalJobs`, with both totals, and by nothing else. Catches a check
/// that matches rows to allocations without comparing what they booked.
#[test]
fn a_row_with_other_cpu_than_the_allocation_booked_is_flagged() {
    assert_eq!(
        lines(&verdict(
            Invariant::UsageTotalsMatchTerminalJobs,
            &skewed_cpu()
        )),
        [
            "usage-totals-match-terminal-jobs at 5000: the usage rows of job sim-job-0 add up to \
             1 allocations, cpu 1001 millis, memory 1048576 bytes, on [host-0], and its \
             allocations freed to 1 allocations, cpu 1000 millis, memory 1048576 bytes, on \
             [host-0]"
        ]
    );
}

/// Asserts that a job with two allocations, whose second row is skewed, is flagged with
/// the totals of both, and that the same two rows unskewed are not. Catches a check that
/// compares one row per job, and one that compares only the first.
#[test]
fn the_rows_of_a_job_with_two_allocations_add_up_to_both() {
    assert_eq!(
        lines(&end(&skewed_second_allocation())),
        [
            "usage-totals-match-terminal-jobs at 5000: the usage rows of job sim-job-0 add up to \
             2 allocations, cpu 3000 millis, memory 2097152 bytes, on [host-0, host-1], and its \
             allocations freed to 2 allocations, cpu 2000 millis, memory 2097152 bytes, on \
             [host-0, host-1]"
        ]
    );
    let sound = at_end(
        vec![freed(0, 1, 0, 100, 300), freed(0, 2, 1, 300, 900)],
        vec![row(0, 0, 300, 1_000), row(0, 1, 900, 1_000)],
    );
    none(&end(&sound));
}

/// Asserts that a row on another host than the allocation's is flagged as a lost row (the
/// allocation has none) and as a row for an allocation that has not ended, and that the totals,
/// which are as many on each side, differ by host and are flagged too. Catches matching by
/// job alone.
#[test]
fn a_row_on_another_host_names_no_ended_allocation() {
    let wrong = at_end(vec![freed(0, 1, 0, 100, 900)], vec![row(0, 1, 900, 1_000)]);
    assert_eq!(
        lines(&end(&wrong)),
        [
            "usage-row-per-ended-allocation at 5000: job sim-job-0 freed on host host-0 at 900 \
             has 0 usage rows, not one",
            "usage-row-per-ended-allocation at 5000: a usage row says job sim-job-0 ended on \
             host host-1 at 900, and no allocation of it was read freed then",
            "usage-totals-match-terminal-jobs at 5000: the usage rows of job sim-job-0 add up to \
             1 allocations, cpu 1000 millis, memory 1048576 bytes, on [host-1], and its \
             allocations freed to 1 allocations, cpu 1000 millis, memory 1048576 bytes, on \
             [host-0]"
        ]
    );
}

/// Asserts that a job with two ended allocations and two rows, both on the first host, is
/// flagged twice over: as a row twice and one lost by the first invariant, and by the
/// totals, which are as many on each side and differ by host. Catches a totals check that
/// compares sums and not the hosts.
#[test]
fn rows_equal_in_number_but_on_the_wrong_hosts_differ_in_total() {
    let both = at_end(
        vec![freed(0, 1, 0, 100, 300), freed(0, 2, 1, 300, 900)],
        vec![row(0, 0, 300, 1_000), row(0, 0, 300, 1_000)],
    );
    assert_eq!(
        flagged(&end(&both)),
        [
            Invariant::UsageRowPerEndedAllocation,
            Invariant::UsageTotalsMatchTerminalJobs
        ]
    );
}

/// Asserts that a run with no allocation and no row is clean, that a run whose cell read no
/// usage history (`None`) is clean whatever it freed, and that allocations not yet freed
/// need no row. Catches a check that flags a missing read as lost rows, and one that
/// demands a row of a booked or releasing allocation.
#[test]
fn nothing_ended_and_nothing_read_are_clean() {
    none(&end(&at_end(vec![], vec![])));
    let unread = Observations::quiet()
        .at(ms(END))
        .with_allocation_read(freed(0, 1, 0, 100, 900), ms(END));
    none(&end(&unread));
    let live = alloc(0, 1, 0, (100, None), AllocationStep::Booked, share());
    let releasing = alloc(1, 2, 1, (100, None), AllocationStep::Releasing, share());
    none(&end(&at_end(vec![live, releasing], vec![])));
}

/// Asserts that an allocation freed whose row is not read yet is flagged at the end and
/// not by a step check: the rows are read once the scenario settled, and a step has none
/// to read. Catches a usage check run at every step, which would flag every free before
/// its row is read.
#[test]
fn a_freed_allocation_whose_row_arrives_later_is_flagged_only_at_the_end() {
    let freed_now = freed(0, 1, 0, 100, 900);
    let before = Observations::quiet()
        .at(ms(1_000))
        .with_allocation_read(freed_now.clone(), ms(1_000));
    none(&step_all(&before));
    let ended = at_end(vec![freed_now], vec![]);
    none(&step_all(&ended));
    assert_eq!(
        flagged(&end(&ended)),
        [Invariant::UsageRowPerEndedAllocation]
    );
}

/// Asserts that a whole-host allocation, which books no CPU and no memory in its row, matches
/// a row of none, and that a job that was read freed twice (the same allocation read
/// again) counts once. Catches a whole host booked as a share, and an allocation counted
/// once per read.
#[test]
fn a_whole_host_matches_a_row_that_books_nothing() {
    let whole = alloc(
        0,
        1,
        0,
        (100, Some(900)),
        AllocationStep::Freed,
        Booked::WholeHost,
    );
    let mut unbooked = row(0, 0, 900, 0);
    unbooked.memory_bytes = 0;
    let once = at_end(vec![whole.clone(), whole.clone()], vec![unbooked]);
    none(&end(&once));
    let skewed = at_end(vec![whole], vec![row(0, 0, 900, 0)]);
    assert_eq!(
        flagged(&end(&skewed)),
        [Invariant::UsageTotalsMatchTerminalJobs]
    );
}

/// Asserts that totals past the largest 64-bit count compare exactly: two allocations of
/// the largest CPU on two hosts match two rows of it, and a row one short does not. Catches
/// a sum that saturates or wraps, which would equal what it should not.
#[test]
fn totals_past_the_largest_count_compare_exactly() {
    let big = Booked::Shape {
        cpu_millis: u64::MAX,
        memory_bytes: 0,
        disk_bytes: 0,
        accelerators: 0,
    };
    let held =
        |a: u8, h: u64, end: u64| alloc(0, a, h, (100, Some(end)), AllocationStep::Freed, big);
    let rows = |cpu: u64| {
        let mut first = row(0, 0, 300, u64::MAX);
        let mut second = row(0, 1, 900, cpu);
        first.memory_bytes = 0;
        second.memory_bytes = 0;
        vec![first, second]
    };
    let sound = at_end(vec![held(1, 0, 300), held(2, 1, 900)], rows(u64::MAX));
    none(&end(&sound));
    let short = at_end(vec![held(1, 0, 300), held(2, 1, 900)], rows(u64::MAX - 1));
    assert_eq!(
        flagged(&end(&short)),
        [Invariant::UsageTotalsMatchTerminalJobs]
    );
}

/// Asserts that an allocation read freed with no free time is matched by a row ended at its
/// creation, as the server names it (the end of a free is its creation when it has no
/// other). Catches a check that cannot match such a row.
#[test]
fn a_freed_allocation_with_no_free_time_is_named_by_its_creation() {
    let unstamped = alloc(0, 1, 0, (100, None), AllocationStep::Freed, share());
    none(&end(&at_end(vec![unstamped], vec![row(0, 0, 100, 1_000)])));
}

/// Asserts that usage rows added one at a time are kept in the order read, the first making
/// the history that was unread, and that a history set in place of them replaces them.
/// Catches a builder that drops a row or keeps an older history.
#[test]
fn usage_rows_are_kept_in_the_order_read() {
    let (first, second) = (row(0, 0, 300, 1_000), row(1, 1, 900, 1_000));
    let seen = Observations::quiet();
    assert_eq!(seen.usage, None);
    let seen = seen
        .with_usage_row(first.clone())
        .with_usage_row(second.clone());
    assert_eq!(seen.usage, Some(vec![first, second.clone()]));
    assert_eq!(
        seen.with_usage(vec![second.clone()]).usage,
        Some(vec![second])
    );
}
