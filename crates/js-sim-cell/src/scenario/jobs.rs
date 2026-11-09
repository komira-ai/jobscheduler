//! What a scenario may state about jobs ([`super::Expect::jobs_read`] and the seven after
//! it): the server's job and allocation tables as the cell read them
//! ([`Observations::job_reads`], [`Observations::allocation_reads`]) and the jobs the
//! agents launched ([`Observations::ledger`]). Each reads what the cell saw, read by read,
//! and never asks the server when it is judged; [`crate::Cell::settle`] reads the tables
//! once more before the end statements are judged.
//!
//! An item's job is the one the job front submits for it, `sim-job-<n>` ([`JOB_PREFIX`]
//! and the item's number). A job's state, and an allocation's step, is the last one the
//! reads showed. A launch of a job is named `<job>/<allocation>` in the ledger, so a job
//! launched again under a second allocation counts twice.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{AllocationId, AllocationStep, JobId, JobState};
use js_sim::text::list;

use crate::jobs::JOB_PREFIX;
use crate::jobs::read::{state_name, step_name};
use crate::observe::{ItemId, Observations};
use crate::text;

/// What a statement about one job says when the reads never showed it, or its allocation.
const UNREAD: &str = "unread";

/// One statement about jobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum JobFact {
    /// Exactly this many jobs were read.
    Read(usize),
    /// Exactly this many jobs were last read in a state that has ended.
    Ended(usize),
    /// Exactly this many jobs were last read in this state.
    In(usize, JobState),
    /// The item's job was last read in this state.
    State(ItemId, JobState),
    /// Exactly this many allocations of the item's job were read.
    Allocations(ItemId, usize),
    /// The item's job's allocation was last read at this step.
    Step(ItemId, AllocationStep),
    /// No allocation was last read booked or releasing.
    NoneLive,
    /// No job was launched more than once.
    LaunchedOnce,
}

/// The statement, what it wanted and what it got, of `fact` over `observations`.
pub(super) fn judged(fact: &JobFact, observations: &Observations) -> [String; 3] {
    let states = last_states(observations);
    match fact {
        JobFact::Read(count) => [
            "jobs-read".to_owned(),
            count.to_string(),
            states.len().to_string(),
        ],
        JobFact::Ended(count) => {
            let ended = states.values().filter(|state| state.is_ended()).count();
            [
                "jobs-ended".to_owned(),
                count.to_string(),
                ended.to_string(),
            ]
        }
        JobFact::In(count, wanted) => {
            let found = states.values().filter(|state| **state == *wanted).count();
            let name = format!("jobs-in({})", state_name(*wanted));
            [name, count.to_string(), found.to_string()]
        }
        JobFact::State(item, wanted) => {
            let job = job_of(*item);
            let state = states.iter().find(|(id, _)| id.as_str() == job);
            let got = state.map_or(UNREAD, |(_, state)| state_name(*state));
            let name = format!("job-state({})", text::item(*item));
            [name, state_name(*wanted).to_owned(), got.to_owned()]
        }
        JobFact::Allocations(item, count) => {
            let job = job_of(*item);
            let reads = observations.allocation_reads.iter();
            let mine = reads.filter(|read| read.row.job.as_str() == job);
            let ids: BTreeSet<AllocationId> = mine.map(|read| read.row.allocation).collect();
            let name = format!("allocations({})", text::item(*item));
            [name, count.to_string(), ids.len().to_string()]
        }
        JobFact::Step(item, wanted) => {
            let job = job_of(*item);
            let mut reads = observations.allocation_reads.iter().rev();
            let last = reads.find(|read| read.row.job.as_str() == job);
            let got = last.map_or(UNREAD, |read| step_name(read.row.step));
            let name = format!("allocation-step({})", text::item(*item));
            [name, step_name(*wanted).to_owned(), got.to_owned()]
        }
        JobFact::NoneLive => {
            let mut rows = BTreeMap::new();
            for read in &observations.allocation_reads {
                rows.insert(read.row.allocation, &read.row);
            }
            let live = rows
                .values()
                .filter(|row| row.step != AllocationStep::Freed);
            let live: Vec<String> = live
                .map(|row| {
                    let step = step_name(row.step);
                    format!("{} {step} on {}", row.job, text::host(&row.host))
                })
                .collect();
            let none = list::<String>(&[], String::clone);
            let got = list(&live, String::clone);
            ["no-allocation-live".to_owned(), none, got]
        }
        JobFact::LaunchedOnce => {
            let mut launches: BTreeMap<&str, usize> = BTreeMap::new();
            let ledger = observations.ledger.iter();
            for (job, _) in ledger.filter_map(|launch| launch.run.as_str().split_once('/')) {
                *launches.entry(job).or_insert(0) += 1;
            }
            let again = launches.iter().filter(|(_, count)| **count > 1);
            let again: Vec<String> = again
                .map(|(job, count)| format!("{job} x{count}"))
                .collect();
            let none = list::<String>(&[], String::clone);
            [
                "each-job-launched-once".to_owned(),
                none,
                list(&again, String::clone),
            ]
        }
    }
}

/// The id of the job the job front submits for `item`.
fn job_of(item: ItemId) -> String {
    format!("{JOB_PREFIX}{}", item.0)
}

/// The last state the reads showed of each job read.
fn last_states(observations: &Observations) -> BTreeMap<&JobId, JobState> {
    let reads = observations.job_reads.iter();
    reads.map(|read| (&read.job, read.state)).collect()
}

#[cfg(test)]
mod tests {
    use js_jobs::StartFailure;
    use js_types::HostId;
    use js_types::sched::FarmTime;

    use super::*;
    use crate::jobs::AllocationRow;
    use crate::observe::RunId;

    fn job(n: u32) -> JobId {
        JobId::parse(&job_of(ItemId(n))).expect("a job id")
    }

    fn host() -> HostId {
        HostId::new("h000").expect("a host id")
    }

    fn at(millis: u64) -> FarmTime {
        FarmTime::from_millis(millis)
    }

    fn row(n: u32, allocation: u8, step: AllocationStep) -> AllocationRow {
        AllocationRow {
            job: job(n),
            allocation: AllocationId::from_bits([allocation; 16]),
            host: host(),
            step,
            created_at: at(0),
            end_reason: None,
            freed_at: None,
            booked: js_jobs::Booked::WholeHost,
        }
    }

    /// What `fact` got over `observations`, and whether that is what it wanted.
    fn got(fact: &JobFact, observations: &Observations) -> (String, bool) {
        let [_, wanted, got] = judged(fact, observations);
        let met = wanted == got;
        (got, met)
    }

    /// Catches: a job counted once per read rather than once, a state other than the last
    /// read taken as the job's, an ended state not counted as ended (or a live one counted),
    /// a job of another item read as the item's, and a job never read shown as anything but
    /// `unread`.
    #[test]
    fn job_statements_read_the_last_state_of_each_job() {
        let timeout = JobState::FailedToStart(StartFailure::StartTimeout);
        let seen = Observations::quiet()
            .with_job_read(job(0), JobState::Queued, at(1))
            .with_job_read(job(1), JobState::Running, at(1))
            .with_job_read(job(0), JobState::Placed, at(2))
            .with_job_read(job(0), timeout, at(3));
        assert_eq!(got(&JobFact::Read(2), &seen), ("2".to_owned(), true));
        assert_eq!(got(&JobFact::Ended(1), &seen), ("1".to_owned(), true));
        assert!(!got(&JobFact::Ended(2), &seen).1);
        let running = JobFact::In(1, JobState::Running);
        assert_eq!(got(&running, &seen), ("1".to_owned(), true));
        assert!(!got(&JobFact::In(1, JobState::Queued), &seen).1);
        let state = JobFact::State(ItemId(0), timeout);
        assert_eq!(
            judged(&state, &seen),
            [
                "job-state(item0)".to_owned(),
                "failed-to-start(start-timeout)".to_owned(),
                "failed-to-start(start-timeout)".to_owned(),
            ]
        );
        let never = JobFact::State(ItemId(7), JobState::Lost);
        assert_eq!(got(&never, &seen), ("unread".to_owned(), false));
        assert_eq!(
            judged(&JobFact::In(0, JobState::Lost), &seen)[0],
            "jobs-in(lost)"
        );
    }

    /// Catches: an allocation counted once per read rather than once per id, an
    /// allocation of another job counted as the item's, a step other than the last read
    /// taken as the allocation's, an allocation never read shown as anything but
    /// `unread`, an allocation read booked and then freed still counted live, and one last
    /// read booked or releasing missed.
    #[test]
    fn allocation_statements_read_the_last_row_of_each_allocation() {
        let seen = Observations::quiet()
            .with_allocation_read(row(0, 1, AllocationStep::Booked), at(1))
            .with_allocation_read(row(1, 2, AllocationStep::Booked), at(1))
            .with_allocation_read(row(0, 1, AllocationStep::Releasing), at(2));
        let one = JobFact::Allocations(ItemId(0), 1);
        assert_eq!(got(&one, &seen), ("1".to_owned(), true));
        let releasing = JobFact::Step(ItemId(0), AllocationStep::Releasing);
        assert_eq!(got(&releasing, &seen), ("releasing".to_owned(), true));
        let never = JobFact::Step(ItemId(5), AllocationStep::Freed);
        assert_eq!(got(&never, &seen), ("unread".to_owned(), false));
        assert_eq!(
            judged(&JobFact::NoneLive, &seen),
            [
                "no-allocation-live".to_owned(),
                "[]".to_owned(),
                "[sim-job-0 releasing on h000; sim-job-1 booked on h000]".to_owned(),
            ]
        );
        let placed_again = seen
            .clone()
            .with_allocation_read(row(0, 1, AllocationStep::Freed), at(3))
            .with_allocation_read(row(1, 2, AllocationStep::Freed), at(3))
            .with_allocation_read(row(0, 3, AllocationStep::Freed), at(4));
        let freed = got(&JobFact::NoneLive, &placed_again);
        assert_eq!(freed, ("[]".to_owned(), true));
        assert_eq!(got(&one, &placed_again), ("2".to_owned(), false));
    }

    /// Catches: a job launched under two allocations not named, one launched once named,
    /// and a launch that names no allocation taken for a job.
    #[test]
    fn a_job_launched_under_two_allocations_is_named() {
        let once = Observations::quiet()
            .with_launch(RunId::new("sim-job-0/aa"), host(), at(1))
            .with_launch(RunId::new("sim-job-1/bb"), host(), at(1))
            .with_launch(RunId::new("lease-run"), host(), at(1))
            .with_launch(RunId::new("lease-run"), host(), at(2));
        assert_eq!(
            judged(&JobFact::LaunchedOnce, &once),
            [
                "each-job-launched-once".to_owned(),
                "[]".to_owned(),
                "[]".to_owned(),
            ]
        );
        let twice = once.with_launch(RunId::new("sim-job-0/cc"), host(), at(3));
        assert_eq!(
            got(&JobFact::LaunchedOnce, &twice),
            ("[sim-job-0 x2]".to_owned(), false)
        );
    }
}
