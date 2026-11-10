//! Lifts the cordon of a host, as an operator does.

use super::Action;
use crate::cell::Cell;

/// Lifts the cordon of the host of agent `index` through the job service's route
/// ([`Cell::uncordon_host`]). An index the cell has no agent for changes nothing.
#[derive(Clone, Debug)]
pub struct UncordonHost(usize);

/// Lifts the cordon of the host of agent `index`, now.
#[must_use]
pub fn uncordon_host(index: usize) -> UncordonHost {
    UncordonHost(index)
}

impl Action for UncordonHost {
    fn run(&self, cell: &mut Cell) {
        cell.uncordon_host(self.0);
    }
}
