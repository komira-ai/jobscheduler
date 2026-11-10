//! What a run asked of the server, counted from outside it: the calls the server answered,
//! the calls the client made, and the changes the cell read in the job tables.
//!
//! The server's own work (the transactions its stores commit, the placement passes its
//! engine runs) has no public count the cell can read, so none is counted here. Each
//! number below bounds it from above instead: a heartbeat answered starts at most one
//! placement pass, and a job-table row changes only when a transaction wrote it. A scenario
//! states an upper bound on each number from its own algorithm (see `tests/scale.rs`), so a
//! change that makes the work grow faster than that, per job or per host, shows as a count
//! past its bound.

use std::collections::BTreeSet;

use js_jobs::JobState;

use crate::bus::Route;
use crate::cell::Cell;
use crate::front::CallKind;

/// The counts of one run, read at any time from its [`Cell`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Load {
    /// Hellos the server answered, whatever the answer.
    pub hellos: u64,
    /// Heartbeats the server answered, whatever the answer. Each is at most one placement
    /// pass of the server's engine.
    pub heartbeats: u64,
    /// Submits the client made (a submit sent again counts again).
    pub submits: u64,
    /// Reads the client made, of a job and of its result.
    pub reads: u64,
    /// Cancels the client made.
    pub cancels: u64,
    /// Changes of a job's state the cell read in the server's job table.
    pub job_changes: u64,
    /// Changes of an allocation row the cell read in the server's allocation table.
    pub allocation_changes: u64,
    /// How many different allocations the cell read.
    pub allocations: u64,
    /// Jobs the cell read ended as failed to start.
    pub failed_starts: u64,
}

impl Load {
    /// The counts of `cell` as it stands.
    #[must_use]
    pub fn of(cell: &Cell) -> Self {
        let calls = |kind| cell.answers().iter().filter(|a| a.kind == kind).count() as u64;
        let served = |route| {
            cell.server()
                .map_or(0, |server| server.answered_total(route))
        };
        let seen = cell.observations();
        let ids: BTreeSet<_> = seen
            .allocation_reads
            .iter()
            .map(|read| &read.row.allocation)
            .collect();
        let failed = seen
            .job_reads
            .iter()
            .filter(|read| matches!(read.state, JobState::FailedToStart(_)));
        Self {
            hellos: served(Route::Hello),
            heartbeats: served(Route::Heartbeat),
            submits: calls(CallKind::Queue),
            reads: calls(CallKind::Read),
            cancels: calls(CallKind::Cancel),
            job_changes: seen.job_reads.len() as u64,
            allocation_changes: seen.allocation_reads.len() as u64,
            allocations: ids.len() as u64,
            failed_starts: failed.count() as u64,
        }
    }

    /// Every call the client made.
    #[must_use]
    pub fn client_calls(&self) -> u64 {
        self.submits + self.reads + self.cancels
    }
}
