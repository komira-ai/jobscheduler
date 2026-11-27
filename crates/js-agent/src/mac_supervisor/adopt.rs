//! A supervisor an earlier process of the agent started, taken back by a later one.
//!
//! The supervisor is its job's parent and needs no agent: when the agent stops, restarts or
//! crashes, the supervisor goes on waiting for the job and writes the exit record when the job
//! ends. A later process cannot wait for it (only a parent can), so it names it by its
//! identity ([`SupervisorId`]: its pid and the time it started, which together name one process
//! for as long as the Mac runs) and reads its records.
//!
//! - **Adoptable** ([`adoptable`]): the record directory holds a whole exit record (the job
//!   ended while no agent ran, and the record says how), or the supervisor is still the process
//!   its identity names. A supervisor that is gone and left no record cannot say how its job
//!   ended, and the run is not adopted.
//! - **The handle** ([`Adopted`]) answers as the handle of a supervisor this process started:
//!   the ready record (the one in the record directory, or the one the caller kept), and the
//!   exit record once it is written. A supervisor found gone with no record ends the run as a
//!   failed setup at [`Step::Spawn`] (its wait of the job said nothing), which no job's exit
//!   is: the runtime reports the run gone.
//!
//! How the start time is read is the caller's ([`StartOf`]): on a Mac `proc_pidinfo`
//! (`macos::start_of`), in tests a fake.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::launch::Supervised;
use super::record::{EXIT, Ended, ExitRecord, READY, Ready, Step, parse_exit, parse_ready};

/// The start time of the live process of a pid, in the unit of the host's process table
/// (microseconds since the Unix epoch on a Mac); `None` when there is no such process or it
/// has ended and waits to be reaped.
pub type StartOf = Arc<dyn Fn(u32) -> Option<u64> + Send + Sync>;

/// How a run ends whose supervisor is gone and left no exit record.
pub const GONE: &str = "the supervisor ended without an exit record";

/// The identity of a supervisor process: its pid and its start time, which together name one
/// process for as long as the host runs (a pid used again has another start time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorId {
    /// The process id.
    pub pid: u32,
    /// When the process started, as [`StartOf`] reads it.
    pub start: u64,
}

impl SupervisorId {
    /// The identity of the live process `pid`, as `start_of` reads it; `None` when it is gone.
    #[must_use]
    pub fn of(pid: u32, start_of: &dyn Fn(u32) -> Option<u64>) -> Option<Self> {
        start_of(pid).map(|start| Self { pid, start })
    }

    /// Whether the process it names still runs: its pid has the same start time.
    #[must_use]
    pub fn alive(&self, start_of: &dyn Fn(u32) -> Option<u64>) -> bool {
        start_of(self.pid) == Some(self.start)
    }
}

/// Whether the run of the supervisor `id`, whose records are in `dir`, can be taken back: its
/// exit record is there whole, or the supervisor still runs.
#[must_use]
pub fn adoptable(dir: &Path, id: SupervisorId, start_of: &dyn Fn(u32) -> Option<u64>) -> bool {
    exit_in(dir).is_some() || id.alive(start_of)
}

/// The exit record in `dir`, when it has been written whole.
fn exit_in(dir: &Path) -> Option<ExitRecord> {
    parse_exit(&fs::read_to_string(dir.join(EXIT)).unwrap_or_default())
}

/// The handle of a supervisor an earlier process started.
pub struct Adopted {
    id: SupervisorId,
    dir: PathBuf,
    ready: Ready,
    start_of: StartOf,
}

impl Adopted {
    /// The supervisor `id`, whose records are in `dir`. Its ready record is the one in `dir`;
    /// `kept` (what the caller recorded of it) when that cannot be read.
    #[must_use]
    pub fn new(id: SupervisorId, dir: PathBuf, kept: Ready, start_of: StartOf) -> Self {
        let read = fs::read_to_string(dir.join(READY)).unwrap_or_default();
        let ready = parse_ready(&read).unwrap_or(kept);
        Self {
            id,
            dir,
            ready,
            start_of,
        }
    }

    /// The record of a supervisor gone with none of its own.
    fn gone(&self) -> ExitRecord {
        ExitRecord {
            ended: Ended::SetupFailed {
                step: Step::Spawn,
                why: GONE.to_owned(),
            },
            job_pid: Some(self.ready.job_pid),
            asid: self.ready.asid,
            usage: None,
        }
    }
}

impl Supervised for Adopted {
    fn pid(&self) -> u32 {
        self.id.pid
    }

    fn wait_ready(&mut self) -> Result<Ready, Ended> {
        Ok(self.ready)
    }

    /// The exit record; `None` while the supervisor runs and has written none; once it is
    /// gone without one, the record of [`GONE`].
    fn try_exit(&mut self) -> io::Result<Option<ExitRecord>> {
        if let Some(record) = exit_in(&self.dir) {
            return Ok(Some(record));
        }
        if self.id.alive(&*self.start_of) {
            return Ok(None);
        }
        // The record may have been written between the first look and the end.
        Ok(Some(exit_in(&self.dir).unwrap_or_else(|| self.gone())))
    }

    fn record_dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
#[path = "adopt_tests.rs"]
mod tests;
