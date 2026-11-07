//! Starts a second server on the server's store.

use super::Action;
use crate::cell::Cell;

/// Starts a second server over the server's store and stops it again if it started
/// ([`Cell::start_second_server`]): refused while the server holds the store's writer
/// lock.
#[derive(Clone, Debug)]
pub struct StartSecondServer;

/// Starts a second server on the store now.
#[must_use]
pub fn start_second_server() -> StartSecondServer {
    StartSecondServer
}

impl Action for StartSecondServer {
    fn run(&self, cell: &mut Cell) {
        cell.start_second_server();
    }
}
