//! The process table a sweep reads and acts on: the operating system's on a Mac, a fake in
//! the tests.

use std::io;

use super::Proc;

/// The processes of the machine, as a sweep sees and ends them.
pub trait ProcessTable {
    /// Every live process (a zombie, which has exited and waits to be reaped, is left out),
    /// with its audit session read when `with_asid` is set (and `None` otherwise, or when it
    /// cannot be read).
    ///
    /// # Errors
    ///
    /// When the table cannot be read: the sweep then stops, not clean, with the reason.
    fn list(&self, with_asid: bool) -> io::Result<Vec<Proc>>;

    /// Ends process `pid` at once (`SIGKILL`). A process already gone is not an error, and
    /// nothing is returned: the next listing shows whether it ended.
    fn kill(&self, pid: i32);

    /// Waits a moment between two rounds, so the processes just killed have ended before the
    /// next listing.
    fn pause(&self);
}
