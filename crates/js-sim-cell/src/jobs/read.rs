//! The cell's read of the server's job and allocation tables, through the in-process
//! handle, into [`Observations::job_reads`] and [`Observations::allocation_reads`].
//!
//! A read lists every job (one scan, in pages of 64 rows) and every live allocation (one scan), and
//! reads one allocation more for each job whose state changed and each allocation that was
//! live at the read before and no longer is (it was freed, or a new placement took its
//! row): so a placement freed between two reads is still read freed. Only what differs from
//! the read before is appended, and written into the trace as one line each (`job <id>
//! <state>`, `allocation <job> <step> <host>`), never with an id the server generated.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::Arc;

use js_jobs::{
    AllocationRecord, AllocationStep, JobId, JobRecord, JobState, JobStore, StartFailure,
};
use js_lease_server::in_process::{InProcess, ServerIds};
use js_state::{ResumeToken, Store};
use js_types::sched::FarmTime;

use super::{AllocationRead, AllocationRow, JobRead};
use crate::observe::Observations;
use crate::server::{ServerNode, virtual_time};

/// The job tables as the server holds them.
type Tables = JobStore<Arc<dyn Store>, ServerIds>;

/// What the cell last read of the server's tables: each job's state and each job's
/// allocation row.
#[derive(Debug, Default)]
pub(crate) struct Reader {
    states: BTreeMap<JobId, JobState>,
    rows: BTreeMap<JobId, AllocationRow>,
}

impl Reader {
    /// Reads the tables of `server` (when it runs with the job service on) at `now` into
    /// `observations`, and returns a trace line for each change.
    pub(crate) fn read(
        &mut self,
        server: Option<&ServerNode>,
        observations: &mut Observations,
        now: FarmTime,
    ) -> Vec<String> {
        let tables = server
            .and_then(ServerNode::server)
            .and_then(InProcess::jobs);
        let Some(tables) = tables else {
            return Vec::new();
        };
        observations.tables_at = Some(now);
        let mut lines = Vec::new();
        let mut look = BTreeSet::new();
        for record in all_jobs(tables) {
            if self.states.get(&record.id) == Some(&record.state) {
                continue;
            }
            self.states.insert(record.id.clone(), record.state);
            lines.push(format!("job {} {}", record.id, state_name(record.state)));
            let read = JobRead {
                job: record.id.clone(),
                state: record.state,
                at: now,
            };
            observations.job_reads.push(read);
            look.insert(record.id);
        }
        let live = tables.live_allocations().unwrap_or_default();
        let live_jobs: BTreeSet<JobId> = live.iter().map(|row| row.job.clone()).collect();
        let gone = self
            .rows
            .iter()
            .filter(|(job, row)| row.step != AllocationStep::Freed && !live_jobs.contains(*job));
        look.extend(gone.map(|(job, _)| job.clone()));
        let looked = look.iter().filter(|job| !live_jobs.contains(*job));
        let looked = looked.filter_map(|job| tables.get_allocation(job).ok().flatten());
        let looked: Vec<AllocationRecord> = looked.collect();
        for row in live.into_iter().chain(looked).map(row_of) {
            if self.rows.get(&row.job) == Some(&row) {
                continue;
            }
            let (job, step, host) = (&row.job, step_name(row.step), row.host.as_str());
            lines.push(format!("allocation {job} {step} {host}"));
            self.rows.insert(row.job.clone(), row.clone());
            observations
                .allocation_reads
                .push(AllocationRead { row, at: now });
        }
        lines
    }
}

/// How many rows of the job table one page of a read holds.
const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(63);

/// Every job of `tables`, [`PAGE`] rows at a time, in id order.
fn all_jobs(tables: &Tables) -> Vec<JobRecord> {
    let page = |resume: Option<&ResumeToken>| {
        let page = tables.list_jobs(None, resume, PAGE).ok();
        page.map(|page| (page.jobs, page.resume))
            .unwrap_or_default()
    };
    let first = Some(page(None));
    let pages = std::iter::successors(first, |(_, next)| {
        next.as_ref().map(|next| page(Some(next)))
    });
    pages.flat_map(|(jobs, _)| jobs).collect()
}

/// An allocation record as the cell observes it, its creation on the virtual clock.
fn row_of(record: AllocationRecord) -> AllocationRow {
    let created = record.created_at.as_duration().as_millis();
    AllocationRow {
        job: record.job,
        allocation: record.allocation_id,
        host: record.host,
        step: record.step,
        created_at: virtual_time(u64::try_from(created).unwrap_or(u64::MAX)),
        end_reason: record.end_reason,
    }
}

/// A job's state as the trace and a violation write it.
pub(crate) fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Placed => "placed",
        JobState::Starting => "starting",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Lost => "lost",
        JobState::FailedToStart(StartFailure::NoCapacity) => "failed-to-start(no-capacity)",
        JobState::FailedToStart(StartFailure::StartTimeout) => "failed-to-start(start-timeout)",
        JobState::FailedToStart(StartFailure::ExclusiveNotReady) => {
            "failed-to-start(exclusive-not-ready)"
        }
        JobState::FailedToStart(StartFailure::AgentRefused) => "failed-to-start(agent-refused)",
    }
}

/// An allocation's step as the trace and a violation write it.
pub(crate) fn step_name(step: AllocationStep) -> &'static str {
    match step {
        AllocationStep::Booked => "booked",
        AllocationStep::Releasing => "releasing",
        AllocationStep::Freed => "freed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: two states, or two steps, spelled alike in the trace, which would hide a
    /// change from a reader of a failing seed's trace lines.
    #[test]
    fn every_state_and_step_has_its_own_name() {
        let states = [
            JobState::Queued,
            JobState::Placed,
            JobState::Starting,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
            JobState::Lost,
            JobState::FailedToStart(StartFailure::NoCapacity),
            JobState::FailedToStart(StartFailure::StartTimeout),
            JobState::FailedToStart(StartFailure::ExclusiveNotReady),
            JobState::FailedToStart(StartFailure::AgentRefused),
        ];
        let names: BTreeSet<&str> = states.into_iter().map(state_name).collect();
        assert_eq!(names.len(), states.len());
        assert_eq!(state_name(JobState::Lost), "lost");
        let steps = [
            AllocationStep::Booked,
            AllocationStep::Releasing,
            AllocationStep::Freed,
        ];
        let names: Vec<&str> = steps.into_iter().map(step_name).collect();
        assert_eq!(names, ["booked", "releasing", "freed"]);
    }

    /// Catches: a read of a server without the job service, or of no server, that writes a
    /// read time or a trace line.
    #[test]
    fn a_server_without_the_job_service_is_not_read() {
        let mut observations = Observations::quiet();
        let lines = Reader::default().read(None, &mut observations, FarmTime::from_millis(5));
        assert!(lines.is_empty());
        assert_eq!(observations.tables_at, None);
    }
}
