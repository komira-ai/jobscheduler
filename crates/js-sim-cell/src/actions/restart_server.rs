//! Restarts the server.

use super::Action;
use crate::cell::Cell;

/// Stops the server and starts it again over the same directory with `--boot resume`,
/// on the same virtual clock, keeping the agents and the client
/// ([`Cell::restart_server`]): the router clones and the in-process server are dropped
/// first, so the new server can lock the state file. A restart the server refuses fails
/// the seed as a failure to start.
#[derive(Clone, Debug)]
pub struct RestartServer;

/// Restarts the server now.
#[must_use]
pub fn restart_server() -> RestartServer {
    RestartServer
}

impl Action for RestartServer {
    fn run(&self, cell: &mut Cell) {
        cell.restart_server();
    }
}
