//! Stops the server.

use super::Action;
use crate::cell::Cell;

/// Stops the server between two inputs, as its process is killed, and reads its store
/// while it is down ([`Cell::stop_server`]); [`super::restart_server`] starts it again.
#[derive(Clone, Debug)]
pub struct StopServer;

/// Stops the server now.
#[must_use]
pub fn stop_server() -> StopServer {
    StopServer
}

impl Action for StopServer {
    fn run(&self, cell: &mut Cell) {
        cell.stop_server();
    }
}
