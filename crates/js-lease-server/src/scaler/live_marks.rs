//! The scale-down marks the scaler puts through [`Live`], kept in the job store's row
//! `hostmark/<host>` when the server holds a job store: every server's placement pass refuses
//! a host whose row is there, and each tick puts the rows on the registry
//! ([`js_registry::Registry::mirror_marks`]), so the registry's marks are their cache.
//!
//! A row is written once the registry has accepted the call, at the version read (or absent),
//! and the read and the write are made again once when another writer changed the row between
//! them. A write that fails, or loses twice, is logged: the registry holds a mark the rows do
//! not, and the next tick's mirror puts it back to what the rows hold.

use js_jobs::{JobStoreError, ScaleMark};
use js_types::HostId;
use tracing::warn;

use super::Live;
use crate::job_store::ServerJobs;
use crate::scaler::Call;

/// How many times a row is read and written before the write is given up.
const TRIES: usize = 2;

/// The mark a registry call leaves on the row: `Some(Some(mark))` to put `mark`,
/// `Some(None)` to take the row off, `None` for a call that is no mark.
pub(super) fn row_of(call: Call) -> Option<Option<ScaleMark>> {
    match call {
        Call::Drain | Call::Retire => Some(Some(ScaleMark::Draining)),
        Call::ReleaseHost | Call::MarkEnding => Some(Some(ScaleMark::Releasing)),
        Call::Undrain => Some(None),
        Call::Cordon => None,
    }
}

/// One read and one write of the row of `host`: whether `want` (a mark, or none to take the
/// row off) is in the store after it, `false` when the row changed between the read and the
/// write. A releasing row is never lowered to draining.
fn write_once(
    jobs: &ServerJobs,
    host: &HostId,
    want: Option<ScaleMark>,
) -> Result<bool, JobStoreError> {
    let read = jobs.read_host_mark(host)?;
    match (want, read) {
        (Some(ScaleMark::Draining), Some((ScaleMark::Releasing, _))) => Ok(true),
        (Some(mark), Some((held, _))) if held == mark => Ok(true),
        (Some(mark), read) => jobs.mark_host(host, mark, read.map(|(_, at)| at)),
        (None, Some((_, at))) => jobs.unmark_host(host, at),
        (None, None) => Ok(true),
    }
}

impl Live {
    /// Writes the row of `host` as `want` says, when the server holds a job store: the mark
    /// the registry took, or none after an undrain or a remove. A blocking call, made from
    /// the scaler's own thread. A failed write is logged and left to the next tick's mirror.
    pub(super) fn keep_mark(&self, host: &HostId, want: Option<ScaleMark>) {
        let Some(jobs) = &self.agents.jobs else {
            return;
        };
        for _ in 0..TRIES {
            match write_once(jobs, host, want) {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    warn!(%host, %error, "cannot write the host's mark row; the next tick puts the registry back to the rows");
                    return;
                }
            }
        }
        warn!(%host, "the host's mark row changed under two writes; the next tick puts the registry back to the rows");
    }

    /// After the registry removed `host` (its machine read back gone): deletes the host's
    /// `node/` row, at the version read, and then its mark row, so no server counts the
    /// machine in its pool's cap or offers it a job while the row would still read live. A
    /// row that changed between the read and the delete is read again, once; a failure is
    /// logged, and the row is forgotten once it is silent long enough and holds nothing
    /// ([`js_jobs::JobStore::forget_silent_nodes`]). While the row stays, the job pass reads
    /// the host present and its live jobs stay as they are (design `multi-instance.md`, G6).
    pub(super) fn forget_removed(&self, host: &HostId) {
        let Some(jobs) = &self.agents.jobs else {
            return;
        };
        let forgot = (0..TRIES).try_fold(false, |done, _| match done {
            true => Ok(true),
            false => match jobs.read_node(host)? {
                Some(row) => jobs.forget_node(host, row.version),
                None => Ok(true),
            },
        });
        match forgot {
            Ok(true) => {}
            Ok(false) => {
                warn!(%host, "the removed host's row changed under two deletes; while it stays, the host's jobs are not settled")
            }
            Err(error) => {
                warn!(%host, %error, "cannot delete the removed host's row; while it stays, the host's jobs are not settled")
            }
        }
        self.keep_mark(host, None);
    }
}
