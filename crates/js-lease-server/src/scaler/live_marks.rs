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
}
