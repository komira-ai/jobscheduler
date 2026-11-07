//! Preempts one machine of the elastic pool's cloud.

use super::Action;
use crate::cell::Cell;

/// Makes the cloud preempt the machine of a number, in launch order from 0
/// ([`Cell::preempt_machine`]): it stops running and billing, its agent is killed in that
/// step, and it reads preempted until it is released. A number the cloud has not launched,
/// or a machine already stopped, changes nothing.
#[derive(Clone, Debug)]
pub struct PreemptMachine(usize);

/// Preempts machine `number` now.
#[must_use]
pub fn preempt_machine(number: usize) -> PreemptMachine {
    PreemptMachine(number)
}

impl Action for PreemptMachine {
    fn run(&self, cell: &mut Cell) {
        cell.preempt_machine(self.0);
    }
}
