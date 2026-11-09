//! Cordons a host, as an operator does.

use super::Action;
use crate::cell::Cell;

/// Cordons the host of agent `index` for `secs` seconds through the job service's route
/// ([`Cell::cordon_host`]): the server places no new job on it while the cordon holds, and
/// the jobs already there go on. An index the cell has no agent for changes nothing.
#[derive(Clone, Debug)]
pub struct CordonHost {
    index: usize,
    secs: u32,
}

/// Cordons the host of agent `index` for `secs` seconds, now.
#[must_use]
pub fn cordon_host(index: usize, secs: u32) -> CordonHost {
    CordonHost { index, secs }
}

impl Action for CordonHost {
    fn run(&self, cell: &mut Cell) {
        cell.cordon_host(self.index, self.secs);
    }
}
