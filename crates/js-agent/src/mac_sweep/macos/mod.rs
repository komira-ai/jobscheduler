//! The macOS process table: the listing through `libproc` ([`procs`]), the audit session
//! through the supervisor's `asid_of` (the one reading of it in the agent), `SIGKILL`, and a
//! short pause between rounds.
//!
//! Nothing here is reached by the Linux test jobs; the decisions it serves are tested there
//! over a fake table.

mod procs;
#[cfg(test)]
mod tests;

use std::io;
use std::thread;
use std::time::Duration;

use super::{Proc, ProcessTable};
use crate::mac_supervisor::macos::audit::asid_of;

/// The pause between two rounds of a sweep: long enough for the processes just killed to
/// have ended (a `SIGKILL` is delivered at once, the exit takes a moment).
const PAUSE: Duration = Duration::from_millis(20);

/// The process table of the Mac the agent runs on.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacTable;

impl ProcessTable for MacTable {
    fn list(&self, with_asid: bool) -> io::Result<Vec<Proc>> {
        procs::list(with_asid, asid_of)
    }

    fn kill(&self, pid: i32) {
        procs::kill(pid);
    }

    fn pause(&self) {
        thread::sleep(PAUSE);
    }
}
