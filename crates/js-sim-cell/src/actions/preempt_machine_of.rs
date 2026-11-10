//! Preempts one machine of one named cloud of the pool.

use super::Action;
use crate::cell::Cell;

/// Makes the cloud named `provider` preempt its machine of a number, in that cloud's launch
/// order from 0 ([`Cell::preempt_machine_of`]), as [`super::preempt_machine`] does for the
/// first cloud. A cloud the cell does not have, a number it has not launched, or a machine
/// already stopped, changes nothing.
#[derive(Clone, Debug)]
pub struct PreemptMachineOf {
    provider: String,
    number: usize,
}

/// Preempts machine `number` of the cloud named `provider` now.
#[must_use]
pub fn preempt_machine_of(provider: &str, number: usize) -> PreemptMachineOf {
    PreemptMachineOf {
        provider: provider.to_owned(),
        number,
    }
}

impl Action for PreemptMachineOf {
    fn run(&self, cell: &mut Cell) {
        cell.preempt_machine_of(&self.provider, self.number);
    }
}
