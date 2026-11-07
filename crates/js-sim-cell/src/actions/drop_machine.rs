//! Drops one machine of the elastic pool's cloud.

use super::Action;
use crate::cell::Cell;

/// Makes the cloud drop the machine of a number, in launch order from 0, as a delete made
/// outside the scheduler ([`Cell::drop_machine`]): it vanishes, a read of it says so, and
/// its agent is killed in that step. A number the cloud has not launched changes nothing.
#[derive(Clone, Debug)]
pub struct DropMachine(usize);

/// Drops machine `number` now.
#[must_use]
pub fn drop_machine(number: usize) -> DropMachine {
    DropMachine(number)
}

impl Action for DropMachine {
    fn run(&self, cell: &mut Cell) {
        cell.drop_machine(self.0);
    }
}
