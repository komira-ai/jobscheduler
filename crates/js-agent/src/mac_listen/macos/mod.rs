//! The macOS listener table: every process's TCP sockets in the listening state, through
//! `libproc` ([`sockets`]).
//!
//! Nothing here is reached by the Linux test jobs; the decisions it serves are tested there
//! over a fake table.

#[cfg(test)]
mod root_tests;
mod sockets;

use std::io;

use super::{Listener, ListenerTable};

/// The listener table of the Mac the agent runs on. It reads other users' processes, so it
/// needs root, which the agent of a native pool is.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacListeners;

impl ListenerTable for MacListeners {
    fn listeners(&self) -> io::Result<Vec<Listener>> {
        sockets::list()
    }
}
