//! The negative cases of `cordon-never-kills-running-job` (in [`NEGATIVE_CASES`], which the
//! parent's table check reads with its own), its correct siblings, and the boundaries of
//! what it reads: when a cordon begins and ends, which jobs it protects, which ends are a
//! job's own, and what the client asked for.
//!
//! In every case host 0 is cordoned at 100 until 1000, and job 0 is running on it, read
//! running at 50, its allocation created at 10, unless the case says otherwise.

use js_jobs::{AllocationId, AllocationStep, Booked, EndReason, JobId, JobState};
use js_sim_cell::{AllocationRow, JobReply};

use super::usage::job;
use super::*;

/// Every negative case of the cordon invariant and the one invariant it must trip, as the
/// parent's `NEGATIVE_CASES`.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::CordonNeverKillsRunningJob, placed_while_cordoned),
    (Invariant::CordonNeverKillsRunningJob, killed_while_cordoned),
    (
        Invariant::CordonNeverKillsRunningJob,
        stopped_while_cordoned,
    ),
];

fn allocation(n: u8) -> AllocationId {
    AllocationId::from_bits([n; 16])
}

/// An allocation of job `j` on host `h` created at `created`, freed at `freed` for
/// `reason`; read at `step`.
fn alloc(
    j: JobId,
    h: u64,
    created: u64,
    freed: Option<u64>,
    reason: Option<EndReason>,
    step: AllocationStep,
) -> AllocationRow {
    AllocationRow {
        job: j,
        allocation: allocation(u8::try_from(created % 200).expect("a small number")),
        host: host(h),
        step,
        created_at: ms(created),
        end_reason: reason,
        freed_at: freed.map(ms),
        booked: Booked::WholeHost,
    }
}

/// Host 0 cordoned at 100 until 1000, and job 0 read running on it at 50 with an
/// allocation created at 10: the history every case starts from.
fn cordoned() -> Observations {
    Observations::quiet()
        .at(ms(2_000))
        .with_cordon(host(0), ms(100), ms(1_000))
        .with_job_read(job(0), JobState::Running, ms(50))
}

/// `base` with job 0's allocation on host 0, created at 10, read at `at` freed at `freed`
/// for `reason`.
fn ending(base: Observations, freed: u64, reason: EndReason, at: u64) -> Observations {
    let row = alloc(
        job(0),
        0,
        10,
        Some(freed),
        Some(reason),
        AllocationStep::Freed,
    );
    base.with_allocation_read(row, ms(at))
}

/// A stop of job 0 read by host 0 at `at`.
fn stop(at: u64) -> JobReply {
    JobReply {
        host: host(0),
        at: ms(at),
        beat_at: ms(at - 10),
        listed: [job(0)].into_iter().collect(),
        starts: Vec::new(),
        stops: [job(0)].into_iter().collect(),
        acks: Vec::new(),
    }
}

/// The lines of the step violations of `observations`.
fn found(observations: &Observations) -> Vec<String> {
    lines(&step_all(observations))
}

// ---- The negative cases. ----

/// Job 1 is given an allocation on host 0 at 200, while it is cordoned.
pub fn placed_while_cordoned() -> Observations {
    let row = alloc(job(1), 0, 200, None, None, AllocationStep::Booked);
    cordoned().with_allocation_read(row, ms(250))
}

/// Job 0, running on host 0 when it was cordoned, is freed at 300 as `host-absent`.
pub fn killed_while_cordoned() -> Observations {
    ending(cordoned(), 300, EndReason::HostAbsent, 350)
}

/// Host 0 reads a stop of job 0 at 200, in the cordon, and the job is freed at 1500, after
/// the cordon, as `refused`.
pub fn stopped_while_cordoned() -> Observations {
    let base = cordoned().with_job_reply(stop(200));
    ending(base, 1_500, EndReason::Refused, 1_600)
}

// ---- The tests. ----

/// Asserts that an allocation created on a cordoned host while the cordon holds is flagged,
/// with the job, the host, the creation and the cordon's bounds, and by nothing else.
/// Catches a placement on a cordoned host the check does not read.
#[test]
fn an_allocation_created_in_a_cordon_is_flagged() {
    assert_eq!(
        found(&placed_while_cordoned()),
        [
            "cordon-never-kills-running-job at 2000: job sim-job-1 was given an allocation on \
             host host-0 at 200, while the host was cordoned from 100 until 1000"
        ]
    );
}

/// Asserts the edges of the cordon for a placement: an allocation created at the moment of
/// the cordon (the placement may have come first in the same millisecond) or at its end, one
/// before it, one after it, and one on another host are not flagged; one at 101 and one at
/// 999 are. Catches an edge a millisecond too wide or too narrow.
#[test]
fn a_placement_is_flagged_only_strictly_inside_the_cordon() {
    let created = |at: u64, on: u64| {
        let row = alloc(job(1), on, at, None, None, AllocationStep::Booked);
        found(&cordoned().with_allocation_read(row, ms(at + 10)))
    };
    for clear in [(99, 0), (100, 0), (1_000, 0), (1_001, 0), (500, 1)] {
        assert_eq!(created(clear.0, clear.1), Vec::<String>::new(), "{clear:?}");
    }
    for flagged in [101, 999] {
        assert_eq!(created(flagged, 0).len(), 1, "{flagged}");
    }
}

/// Asserts that a job running when its host was cordoned and ended by the server for a
/// reason that is not its own is flagged, with the reason; and that each reason of its own
/// (exit, wall limit, memory limit) is not. Catches a cordon that is read as a licence to
/// end a running job, and a check that flags a job's own ends.
#[test]
fn a_running_job_ended_for_another_reason_is_flagged() {
    assert_eq!(
        found(&killed_while_cordoned()),
        [
            "cordon-never-kills-running-job at 2000: job sim-job-0 was running on host host-0 \
             when it was cordoned at 100 and ended at 300 as host-absent while the cordon held \
             until 1000"
        ]
    );
    for own in [EndReason::Exited, EndReason::WallLimit, EndReason::Memory] {
        assert_eq!(
            found(&ending(cordoned(), 300, own, 350)),
            Vec::<String>::new()
        );
    }
    for other in [
        EndReason::HostAbsent,
        EndReason::FailedPrepare,
        EndReason::StartTimeout,
        EndReason::Refused,
        EndReason::Cancelled,
    ] {
        let seen = found(&ending(cordoned(), 300, other, 350));
        assert_eq!(seen.len(), 1, "{other:?}");
    }
}

/// Asserts that a cancel the client asked for before the end excuses a `cancelled` end (the
/// job front names the job of item 0 `sim-job-0`), that one asked after the end does not,
/// and that a job the front did not submit (its name is not an item's) has no cancel to
/// excuse it. Catches a check that flags a cancel of the client's, one that excuses a
/// cancel asked for too late, and one that reads any job as item 0.
#[test]
fn a_cancel_the_client_asked_for_is_not_a_kill() {
    let cancelled = |asked: Option<u64>| {
        let base = match asked {
            Some(at) => cordoned().with_cancel(ItemId(0), ms(at)),
            None => cordoned(),
        };
        found(&ending(base, 300, EndReason::Cancelled, 350))
    };
    assert_eq!(cancelled(Some(250)), Vec::<String>::new());
    assert_eq!(cancelled(Some(300)), Vec::<String>::new());
    assert_eq!(cancelled(Some(301)).len(), 1);
    assert_eq!(cancelled(None).len(), 1);
    let other = JobId::parse("batch-0").expect("a job id");
    let numbered = JobId::parse("sim-job-x").expect("a job id");
    for name in [other, numbered] {
        let base = Observations::quiet()
            .at(ms(2_000))
            .with_cordon(host(0), ms(100), ms(1_000))
            .with_job_read(name.clone(), JobState::Running, ms(50))
            .with_cancel(ItemId(0), ms(150));
        let row = alloc(
            name,
            0,
            10,
            Some(300),
            Some(EndReason::Cancelled),
            AllocationStep::Freed,
        );
        assert_eq!(found(&base.with_allocation_read(row, ms(350))).len(), 1);
    }
}

/// Asserts that an end after the cordon lapsed, an end before the cordon was set, and an
/// end of a job the cell never read running are not flagged: the cordon is not a reason
/// the check reads into them. Catches a check that holds a host to its jobs for ever, one
/// that reads a job ended before the cordon as running through it, and one that protects a
/// job that was not known to run.
#[test]
fn ends_outside_the_cordon_or_of_a_job_not_running_are_not_flagged() {
    let none = Vec::<String>::new();
    assert_eq!(
        found(&ending(cordoned(), 1_000, EndReason::HostAbsent, 1_050)),
        none
    );
    assert_eq!(
        found(&ending(cordoned(), 1_500, EndReason::HostAbsent, 1_550)),
        none
    );
    assert_eq!(
        found(&ending(cordoned(), 100, EndReason::HostAbsent, 150)),
        none
    );
    assert_eq!(
        found(&ending(cordoned(), 90, EndReason::HostAbsent, 95)),
        none
    );
    let unread = Observations::quiet()
        .at(ms(2_000))
        .with_cordon(host(0), ms(100), ms(1_000));
    assert_eq!(
        found(&ending(unread, 300, EndReason::HostAbsent, 350)),
        none
    );
    let late = Observations::quiet()
        .at(ms(2_000))
        .with_cordon(host(0), ms(100), ms(1_000))
        .with_job_read(job(0), JobState::Running, ms(101));
    assert_eq!(found(&ending(late, 300, EndReason::HostAbsent, 350)), none);
    assert_eq!(found(&cordoned()), none);
}

/// Asserts that a job on another host is not held by this host's cordon, whether it is
/// placed or ended there. Catches a check that reads a cordon of one host over every host.
#[test]
fn another_hosts_jobs_are_not_held_by_the_cordon() {
    let row = alloc(
        job(0),
        1,
        10,
        Some(300),
        Some(EndReason::HostAbsent),
        AllocationStep::Freed,
    );
    assert_eq!(
        found(&cordoned().with_allocation_read(row, ms(350))),
        Vec::<String>::new()
    );
}

/// Asserts that an end first read as releasing, with no free time yet, is dated by that
/// read: flagged when the read is inside the cordon, not when it is after it. Catches a
/// check that waits for the free, which a host that never confirms leaves unread, and one
/// that dates the end by the cell's last read.
#[test]
fn an_end_still_releasing_is_dated_by_the_read_that_showed_it() {
    let releasing = |at: u64| {
        let row = alloc(
            job(0),
            0,
            10,
            None,
            Some(EndReason::HostAbsent),
            AllocationStep::Releasing,
        );
        found(&cordoned().with_allocation_read(row, ms(at)))
    };
    assert_eq!(releasing(300).len(), 1);
    assert_eq!(releasing(1_100), Vec::<String>::new());
}

/// Asserts that a stop the host read in the cordon is flagged, with the job and the end, when
/// the job ended after the cordon for a reason that is not its own; that the end is
/// flagged only when it is in the cordon; and that a stop outside the cordon, a stop of a
/// job that ended as it exited or as the client cancelled it, and a stop of a job that is
/// still booked (no end to name) are not. Catches a stop that is read as the job's end
/// when it is a cleanup after its own, and a check that ignores the stops.
#[test]
fn a_stop_in_the_cordon_is_flagged_unless_the_job_ended_its_own_way() {
    assert_eq!(
        found(&stopped_while_cordoned()),
        [
            "cordon-never-kills-running-job at 2000: host host-0 was sent a stop of job \
             sim-job-0 at 200, which was running there when the host was cordoned at 100, and \
             the job ended as refused with no cancel of the client's before the stop"
        ]
    );
    let with = |reply: JobReply, reason: Option<EndReason>| {
        let base = cordoned().with_job_reply(reply);
        let row = alloc(job(0), 0, 10, Some(1_500), reason, AllocationStep::Freed);
        found(&base.with_allocation_read(row, ms(1_600)))
    };
    let none = Vec::<String>::new();
    assert_eq!(with(stop(1_000), Some(EndReason::Refused)), none);
    assert_eq!(with(stop(99), Some(EndReason::Refused)), none);
    assert_eq!(with(stop(200), Some(EndReason::Exited)), none);
    assert_eq!(with(stop(200), None), none);
    let mut other = stop(200);
    other.host = host(1);
    assert_eq!(with(other, Some(EndReason::Refused)), none);
    let asked = |at: u64| {
        let base = cordoned()
            .with_cancel(ItemId(0), ms(at))
            .with_job_reply(stop(200));
        let row = alloc(
            job(0),
            0,
            10,
            Some(1_500),
            Some(EndReason::Cancelled),
            AllocationStep::Freed,
        );
        found(&base.with_allocation_read(row, ms(1_600)))
    };
    assert_eq!(asked(150), none);
    assert_eq!(asked(200), none);
    assert_eq!(asked(300).len(), 1);
}

/// Asserts what a cordon's window is: it ends at its `until`; an uncordon of the host after
/// the cordon ends it there (and one before it does not); a second cordon renews it, past
/// the first's end; and one made after the first lapsed is a second window. Catches a check
/// that reads a cordon to its `until` after the operator lifted it, one that lets an
/// uncordon of before the cordon lift it, and one that does not renew.
#[test]
fn a_cordon_ends_at_its_length_or_its_uncordon_and_a_renewal_extends_it() {
    let placed = |base: Observations, at: u64| {
        let row = alloc(job(1), 0, at, None, None, AllocationStep::Booked);
        found(&base.with_allocation_read(row, ms(at + 10)))
    };
    let none = Vec::<String>::new();
    let lifted = cordoned().with_uncordon(host(0), ms(400));
    assert_eq!(placed(lifted.clone(), 300).len(), 1);
    assert_eq!(placed(lifted.clone(), 400), none);
    assert_eq!(placed(lifted, 500), none);
    let earlier = cordoned().with_uncordon(host(0), ms(50));
    assert_eq!(placed(earlier, 500).len(), 1);
    let elsewhere = cordoned().with_uncordon(host(1), ms(400));
    assert_eq!(placed(elsewhere, 500).len(), 1);
    let renewed = cordoned().with_cordon(host(0), ms(900), ms(2_000));
    assert_eq!(placed(renewed.clone(), 1_500).len(), 1);
    assert_eq!(placed(renewed, 2_500), none);
    let again = cordoned().with_cordon(host(0), ms(1_500), ms(1_800));
    assert_eq!(placed(again.clone(), 1_200), none);
    assert_eq!(placed(again, 1_600).len(), 1);
}

/// Asserts that a job still running through the cordon (its allocation booked, no end read) is
/// not flagged, however long the cordon holds. Catches a check that reads a cordoned
/// host's job with no end as ended.
#[test]
fn a_job_still_running_through_the_cordon_is_not_flagged() {
    let row = alloc(job(0), 0, 10, None, None, AllocationStep::Booked);
    let running = cordoned().with_allocation_read(row, ms(500));
    assert_eq!(found(&running), Vec::<String>::new());
}

/// Asserts that a run with no cordon is clean whatever the allocations and stops are, and
/// that a cordon with no allocation read is. Catches a check that flags without a cordon.
#[test]
fn no_cordon_flags_nothing() {
    let row = alloc(
        job(0),
        0,
        10,
        Some(300),
        Some(EndReason::HostAbsent),
        AllocationStep::Freed,
    );
    let plain = Observations::quiet()
        .at(ms(2_000))
        .with_job_read(job(0), JobState::Running, ms(50))
        .with_job_reply(stop(200))
        .with_allocation_read(row, ms(350));
    assert_eq!(found(&plain), Vec::<String>::new());
    assert_eq!(found(&cordoned()), Vec::<String>::new());
}
