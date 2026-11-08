//! The job invariants' negative cases (in [`NEGATIVE_CASES`], which the parent's table
//! check reads with its own) and their correct siblings, and the boundaries of what each
//! reads.

use js_jobs::{AllocationId, AllocationStep, EndReason, JobId, JobState};
use js_sim_cell::{AllocationRow, JobBeat, JobReply, JobStartSeen, ServerView};

use super::*;

/// Every negative case of a job invariant and the one invariant it must trip, as the
/// parent's `NEGATIVE_CASES`.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::OneAllocationPerJob, two_booked_allocations),
    (Invariant::OneAllocationPerJob, one_job_on_two_agents),
    (Invariant::LostIsTerminal, lost_then_running),
    (Invariant::LostIsTerminal, lost_job_listed_without_a_stop),
    (Invariant::FreedOnlyWhenConfirmed, freed_on_silence),
    (
        Invariant::NoStartWithoutAllocation,
        start_with_no_allocation,
    ),
    (Invariant::ResultKeptUntilAcknowledged, result_dropped),
    (
        Invariant::ResultKeptUntilAcknowledged,
        result_accepted_twice,
    ),
];

/// `others` followed by this module's negative cases: every row the parent's table check
/// reads.
pub(crate) fn table<'a>(
    others: impl Iterator<Item = &'a NegativeCase>,
) -> impl Iterator<Item = &'a NegativeCase> {
    others.chain(NEGATIVE_CASES)
}

fn job(n: u64) -> JobId {
    JobId::parse(&format!("sim-job-{n}")).expect("a job id")
}

fn allocation(n: u8) -> AllocationId {
    AllocationId::from_bits([n; 16])
}

/// Allocation `a` of job `j` on host `h`, at `step`, committed at 100.
fn row(j: u64, a: u8, h: u64, step: AllocationStep) -> AllocationRow {
    AllocationRow {
        job: job(j),
        allocation: allocation(a),
        host: host(h),
        step,
        created_at: ms(100),
        end_reason: None,
    }
}

/// A heartbeat of host `h` at `at` listing `running` without a result and `results` with
/// one.
fn beat(h: u64, at: u64, running: &[u64], results: &[u64]) -> JobBeat {
    JobBeat {
        host: host(h),
        at: ms(at),
        running: running.iter().map(|n| job(*n)).collect(),
        results: results.iter().map(|n| job(*n)).collect(),
        retired: BTreeSet::new(),
    }
}

/// An answer host `h` read at `at` to its heartbeat of `beat_at`, which listed `listed`.
fn reply(h: u64, at: u64, beat_at: u64, listed: &[u64]) -> JobReply {
    JobReply {
        host: host(h),
        at: ms(at),
        beat_at: ms(beat_at),
        listed: listed.iter().map(|n| job(*n)).collect(),
        starts: Vec::new(),
        stops: BTreeSet::new(),
        acks: Vec::new(),
    }
}

/// The lines of the step violations of `observations` that name `invariant`.
fn of(invariant: Invariant, observations: &Observations) -> Vec<String> {
    let found = step_all(observations);
    let named = found.into_iter().filter(|v| v.invariant == invariant);
    lines(&named.collect::<Vec<_>>())
}

/// Asserts that `found` holds no line.
fn clear(found: Vec<String>) {
    assert_eq!(found, Vec::<String>::new());
}

// ---- The negative cases. ----

/// Job 0 read with allocations 1 and 2 both booked.
pub fn two_booked_allocations() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Booked), ms(200))
        .with_allocation_read(row(0, 2, 1, AllocationStep::Booked), ms(250))
}

/// Job 0 runs on host 0 and on host 1 at once, by their agents' truth.
pub fn one_job_on_two_agents() -> Observations {
    let agent = |a| AgentTruth::new(capacity()).with_job(job(0), allocation(a), res(1_000, GIB));
    Observations::quiet()
        .at(ms(300))
        .with_agent(host(0), agent(1))
        .with_agent(host(1), agent(2))
}

/// Job 0 read lost at 100, then running at 200.
pub fn lost_then_running() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_job_read(job(0), JobState::Lost, ms(100))
        .with_job_read(job(0), JobState::Running, ms(200))
}

/// Job 0 read lost at 100; host 0 listed it running at 150 and the answer carried no stop.
pub fn lost_job_listed_without_a_stop() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_job_read(job(0), JobState::Lost, ms(100))
        .with_job_reply(reply(0, 160, 150, &[0]))
}

/// Allocation 1 of job 0 on host 0, which the server lists live, read booked at 200 and
/// freed at 5000 with no heartbeat of host 0 in between.
pub fn freed_on_silence() -> Observations {
    Observations::quiet()
        .at(ms(5_000))
        .with_lost(host(0), false)
        .with_allocation_read(row(0, 1, 0, AllocationStep::Booked), ms(200))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Freed), ms(5_000))
}

/// Host 0 read a start of job 0 under allocation 1 at 300; the tables read at 400 hold no
/// such allocation.
pub fn start_with_no_allocation() -> Observations {
    let mut answer = reply(0, 300, 290, &[]);
    answer.starts.push(JobStartSeen {
        job: job(0),
        allocation: allocation(1),
        refused: false,
    });
    let mut observations = Observations::quiet().at(ms(400)).with_job_reply(answer);
    observations.tables_at = Some(ms(400));
    observations
}

/// Host 0 made job 0's result at 100 and sent a heartbeat without it at 200, having read no
/// verdict on it.
pub fn result_dropped() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_result_made(host(0), job(0), ms(100))
        .with_job_beat(beat(0, 200, &[], &[]))
}

/// Job 0's result accepted on host 0 at 300 and again at 400.
pub fn result_accepted_twice() -> Observations {
    let accepted = |at| {
        let mut answer = reply(0, at, at - 10, &[]);
        answer.acks.push((job(0), Verdict::Accepted));
        answer
    };
    Observations::quiet()
        .at(ms(500))
        .with_job_reply(accepted(300))
        .with_job_reply(accepted(400))
}

// ---- The tests. ----

/// Asserts that a job read with two booked allocations is flagged as
/// `OneAllocationPerJob`, naming both, and that a second allocation read after the first
/// was read freed is not, nor are two jobs with an allocation each. Catches a check that
/// reads one allocation per job only, and one that counts freed allocations as booked.
#[test]
fn two_booked_allocations_for_one_job_are_flagged() {
    assert_eq!(
        lines(&step_all(&two_booked_allocations())),
        [format!(
            "one-allocation-per-job at 300: job sim-job-0 has allocations {} and {} booked at once",
            allocation(1),
            allocation(2)
        )]
    );
    let requeued = Observations::quiet()
        .at(ms(300))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Booked), ms(200))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Freed), ms(220))
        .with_allocation_read(row(0, 2, 1, AllocationStep::Booked), ms(250));
    none(&step_all(&requeued));
    let two_jobs = Observations::quiet()
        .at(ms(300))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Booked), ms(200))
        .with_allocation_read(row(1, 2, 0, AllocationStep::Booked), ms(200));
    none(&step_all(&two_jobs));
}

/// Asserts the other halves of `OneAllocationPerJob`: an allocation id read for a second
/// job, and one read booked again after it was read freed, are flagged; a job run on two
/// hosts by their agents' truth is flagged, and not once the first host stops running it.
/// Catches an allocation id reused for a new placement, and a job started on a second host
/// while the first still runs it.
#[test]
fn an_allocation_reused_or_revived_and_a_job_on_two_hosts_are_flagged() {
    let moved = Observations::quiet()
        .at(ms(300))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Booked), ms(200))
        .with_allocation_read(row(1, 1, 0, AllocationStep::Booked), ms(250));
    let moved_line = format!(
        "one-allocation-per-job at 300: allocation {} of job sim-job-0 on host host-0 was read at 250 for job sim-job-1 on host host-0",
        allocation(1)
    );
    assert_eq!(lines(&step_all(&moved)), [moved_line]);
    let revived = Observations::quiet()
        .at(ms(300))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Freed), ms(200))
        .with_allocation_read(row(0, 1, 0, AllocationStep::Releasing), ms(250));
    let revived_line = format!(
        "one-allocation-per-job at 300: allocation {} of job sim-job-0 was read releasing at 250 after it was read freed",
        allocation(1)
    );
    assert_eq!(lines(&step_all(&revived)), [revived_line]);
    assert_eq!(
        lines(&step_all(&one_job_on_two_agents())),
        ["one-allocation-per-job at 300: job sim-job-0 runs on host host-0 and on host host-1"]
    );
    let running = |a| AgentTruth::new(capacity()).with_job(job(0), allocation(a), res(1_000, GIB));
    let first = Observations::quiet()
        .at(ms(100))
        .with_agent(host(0), running(1));
    let moved_on = Observations::quiet()
        .at(ms(300))
        .with_agent(host(0), AgentTruth::new(capacity()))
        .with_agent(host(1), running(2));
    let mut checker = Checker::new();
    none(&checker.step(&first, &Touched::nothing().agent(host(0))));
    none(&checker.step(&moved_on, &Touched::nothing().agent(host(1))));
}

/// Asserts that a job read lost and then running is flagged as `LostIsTerminal`, and that a
/// job read running and then lost, or read lost twice, is not. Catches a server that
/// revives a lost job.
#[test]
fn a_lost_job_that_reads_running_again_is_flagged() {
    assert_eq!(
        lines(&step_all(&lost_then_running())),
        ["lost-is-terminal at 300: job sim-job-0 read running at 200 after it read lost at 100"]
    );
    let lost_last = Observations::quiet()
        .at(ms(300))
        .with_job_read(job(0), JobState::Running, ms(100))
        .with_job_read(job(0), JobState::Lost, ms(200))
        .with_job_read(job(0), JobState::Lost, ms(250));
    none(&step_all(&lost_last));
}

/// Asserts that an answer to a heartbeat sent after the job read lost and listing it
/// running is flagged as `LostIsTerminal` unless it carries a stop of it, and that one to
/// a heartbeat sent before the job read lost, or listing another job, is not. Catches a
/// server that lets a host run a job it declared lost.
#[test]
fn a_lost_job_a_host_lists_later_is_answered_with_a_stop() {
    assert_eq!(
        lines(&step_all(&lost_job_listed_without_a_stop())),
        [
            "lost-is-terminal at 300: host host-0 listed job sim-job-0 at 150, after it read lost at 100, and the answer carried no stop of it"
        ]
    );
    let mut stopped = reply(0, 160, 150, &[0]);
    stopped.stops.insert(job(0));
    let lost = || {
        Observations::quiet()
            .at(ms(300))
            .with_job_read(job(0), JobState::Lost, ms(100))
    };
    none(&step_all(&lost().with_job_reply(stopped)));
    none(&step_all(&lost().with_job_reply(reply(0, 160, 90, &[0]))));
    none(&step_all(&lost().with_job_reply(reply(0, 160, 150, &[1]))));
}

/// Asserts that an allocation freed with no heartbeat of its host since it was committed
/// is flagged as `FreedOnlyWhenConfirmed` while the server lists the host, and is not when
/// a heartbeat in between left the job out, or named it retired, or the server no longer
/// lists the host, or it ended as a failed whole-host prepare before any launch; and that a
/// heartbeat that listed the job, one sent before the allocation was committed, one sent
/// before its host read the job's start (unless the host refused that start, as a start
/// after its stop), and one sent before its host read the verdict on the
/// job's result, is no confirmation, nor a failed prepare whose placement was launched. Catches a server that
/// frees an allocation on silence, or on a listing it cannot have read.
#[test]
fn an_allocation_freed_on_silence_is_flagged() {
    let line = format!(
        "freed-only-when-confirmed at 5000: allocation {} of job sim-job-0 on host host-0 was read freed at 5000 with no listing of the host since 100 that left the job out, while the server lists the host",
        allocation(1)
    );
    assert_eq!(lines(&step_all(&freed_on_silence())), [line.as_str()]);
    let flagged = |o: &Observations| of(Invariant::FreedOnlyWhenConfirmed, o);
    let confirmed = freed_on_silence().with_job_beat(beat(0, 3_000, &[], &[]));
    clear(flagged(&confirmed));
    let mut retired = beat(0, 3_000, &[], &[]);
    retired.retired.insert(job(0));
    clear(flagged(&freed_on_silence().with_job_beat(retired)));
    let absent = Observations {
        server: ServerView::default(),
        ..freed_on_silence()
    };
    clear(flagged(&absent));
    let still_listed = freed_on_silence().with_job_beat(beat(0, 3_000, &[0], &[]));
    assert_eq!(flagged(&still_listed), [line.as_str()]);
    let with_result = freed_on_silence().with_job_beat(beat(0, 3_000, &[], &[0]));
    assert_eq!(flagged(&with_result), [line.as_str()]);
    let before = freed_on_silence().with_job_beat(beat(0, 50, &[], &[]));
    assert_eq!(flagged(&before), [line.as_str()]);
    let other_host = freed_on_silence().with_job_beat(beat(1, 3_000, &[], &[]));
    assert_eq!(flagged(&other_host), [line.as_str()]);
    let mut prepare = row(0, 1, 0, AllocationStep::Freed);
    prepare.end_reason = Some(EndReason::FailedPrepare);
    let failed = Observations::quiet()
        .at(ms(5_000))
        .with_lost(host(0), false)
        .with_allocation_read(prepare, ms(5_000));
    clear(flagged(&failed));
    let mut started = reply(0, 2_000, 1_900, &[]);
    started.starts.push(JobStartSeen {
        job: job(0),
        allocation: allocation(1),
        refused: false,
    });
    let read_start = || freed_on_silence().with_job_reply(started.clone());
    let before_start = read_start().with_job_beat(beat(0, 1_500, &[], &[]));
    assert_eq!(flagged(&before_start), [line.as_str()]);
    clear(flagged(&read_start().with_job_beat(beat(
        0,
        2_500,
        &[],
        &[],
    ))));
    let mut refused = started.clone();
    refused.starts[0].refused = true;
    let read_refused = freed_on_silence().with_job_reply(refused);
    clear(flagged(&read_refused.with_job_beat(beat(
        0,
        1_500,
        &[],
        &[],
    ))));
    let mut acked = reply(0, 3_000, 2_900, &[]);
    acked.acks.push((job(0), Verdict::Accepted));
    let read_ack = || freed_on_silence().with_job_reply(acked.clone());
    let before_ack = read_ack().with_job_beat(beat(0, 2_500, &[], &[]));
    assert_eq!(flagged(&before_ack), [line.as_str()]);
    clear(flagged(&read_ack().with_job_beat(beat(0, 3_500, &[], &[]))));
    let launched = format!("sim-job-0/{}", allocation(1));
    let ran = failed.with_launch(RunId::new(launched), host(0), ms(150));
    assert_eq!(flagged(&ran).len(), 1);
}

/// Asserts that a start an agent read is flagged as `NoStartWithoutAllocation` when the
/// tables read after it hold no allocation of that id, or hold it for another host or
/// another job, or committed after the start was read; that it is not flagged when the
/// allocation was committed before; and that it is not judged until the tables are read
/// after it. Catches a server that sends a start before it commits the allocation, or for
/// an allocation on another host.
#[test]
fn a_start_with_no_committed_allocation_is_flagged() {
    let line = format!(
        "no-start-without-allocation at 400: host host-0 was sent a start of job sim-job-0 under allocation {} at 300 that no allocation committed before it names",
        allocation(1)
    );
    assert_eq!(
        lines(&step_all(&start_with_no_allocation())),
        [line.as_str()]
    );
    let flagged = |row: AllocationRow| {
        let read = start_with_no_allocation().with_allocation_read(row, ms(400));
        of(Invariant::NoStartWithoutAllocation, &read)
    };
    clear(flagged(row(0, 1, 0, AllocationStep::Booked)));
    assert_eq!(
        flagged(row(0, 1, 1, AllocationStep::Booked)),
        [line.as_str()]
    );
    assert_eq!(
        flagged(row(1, 1, 0, AllocationStep::Booked)),
        [line.as_str()]
    );
    let mut late = row(0, 1, 0, AllocationStep::Booked);
    late.created_at = ms(350);
    assert_eq!(flagged(late), [line.as_str()]);
    let mut unread = start_with_no_allocation();
    unread.tables_at = Some(ms(300));
    none(&step_all(&unread));
    unread.tables_at = None;
    none(&step_all(&unread));
}

/// Asserts that a heartbeat sent after a result was made, without it and before any verdict
/// on it, is flagged as `ResultKeptUntilAcknowledged`; and that one listing it, one sent
/// after a verdict on it was read, one sent after the agent's process was killed, one sent
/// at the instant it was made, and one of another host, are not. Catches an agent that
/// drops a result before the server acknowledged it.
#[test]
fn a_result_dropped_before_its_ack_is_flagged() {
    assert_eq!(
        lines(&step_all(&result_dropped())),
        [
            "result-kept-until-acknowledged at 300: host host-0 sent a heartbeat at 200 without the result of job sim-job-0 it made at 100, before any answer acknowledged it"
        ]
    );
    let made = || {
        Observations::quiet()
            .at(ms(300))
            .with_result_made(host(0), job(0), ms(100))
    };
    none(&step_all(&made().with_job_beat(beat(0, 200, &[], &[0]))));
    let mut acked = reply(0, 150, 140, &[]);
    acked.acks.push((job(0), Verdict::Duplicate));
    let after_ack = made().with_job_reply(acked);
    none(&step_all(&after_ack.with_job_beat(beat(0, 200, &[], &[]))));
    let killed = made().with_kill(host(0), ms(150));
    none(&step_all(&killed.with_job_beat(beat(0, 200, &[], &[]))));
    none(&step_all(&made().with_job_beat(beat(0, 100, &[], &[]))));
    none(&step_all(&made().with_job_beat(beat(1, 200, &[], &[]))));
}

/// Asserts that a job's result accepted twice is flagged as `ResultKeptUntilAcknowledged`,
/// naming both, and that one accepted once and then answered duplicate is not. Catches a
/// server that stores a resent result a second time.
#[test]
fn a_result_accepted_twice_is_flagged() {
    assert_eq!(
        lines(&step_all(&result_accepted_twice())),
        [
            "result-kept-until-acknowledged at 500: the result of job sim-job-0 was accepted twice: on host host-0 at 300 and on host host-0 at 400"
        ]
    );
    let mut first = reply(0, 300, 290, &[]);
    first.acks.push((job(0), Verdict::Accepted));
    let mut again = reply(0, 400, 390, &[]);
    again.acks.push((job(0), Verdict::Duplicate));
    let once = Observations::quiet()
        .at(ms(500))
        .with_job_reply(first)
        .with_job_reply(again);
    none(&step_all(&once));
}

/// Asserts that a job's resources count in its host's capacity beside its runs: a run and a
/// job needing 5 cores of 4 are flagged as `NoDoubleBooking`, naming both. Catches a
/// capacity check that does not see the jobs an agent runs.
#[test]
fn a_job_counts_in_its_hosts_capacity() {
    let agent = AgentTruth::new(capacity())
        .with_run(run(0), lease(0), res(2_000, GIB))
        .with_job(job(0), allocation(1), res(3_000, GIB));
    let observations = Observations::quiet().at(ms(300)).with_agent(host(0), agent);
    assert_eq!(
        lines(&step_all(&observations)),
        [
            "no-double-booking at 300: host host-0 runs [run \"run-0\"; job sim-job-0] needing res(cpu=5000 mem=2147483648 gpus=0) beyond its capacity res(cpu=4000 mem=8589934592 gpus=1) (over on cpu)"
        ]
    );
}
