//! Whether the jobs are at a point where the agent's process may end and leave them to the
//! next one (see [`crate::lifecycle::handover`]).

use super::Lifecycle;
use super::job::Stage;

impl Lifecycle {
    /// Whether the next process of the agent can take every job over as it stands (see
    /// [`crate::lifecycle::handover`]): each one is running with no stop asked of it, blocked by the
    /// taint, or freed and kept only for its result. No start is under way (a job that waits
    /// for its images or providers, or whose container is being made, would be lost at the
    /// next start: nothing of it is recorded as running), no stop is (a stop the next process
    /// does not know of would not be finished), and no finished job's cleanup has begun (its
    /// checks are owed from its first step until they have run, and a record left owed is a
    /// check the next process must run at its start, which taints the host when it cannot run
    /// it or the check finds something; see `owed`).
    pub fn quiet_for_handover(&self) -> bool {
        self.jobs.iter().all(|job| match job.stage {
            Stage::Running(_) => job.cause.is_none(),
            // The checks of both have run: the record went with them, or, for a blocked
            // job whose taint could not be written, stays beside a taint the next start reads.
            Stage::Blocked | Stage::Freed => true,
            Stage::Preparing(_) | Stage::Removing(_) | Stage::Scratch | Stage::Checks => false,
        })
    }
}
