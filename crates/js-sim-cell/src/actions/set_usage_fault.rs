//! Makes the cell read the usage history as a store with a defect would tell it.

use super::Action;
use crate::cell::Cell;
use crate::usage::UsageReadFault;

/// Sets the defect the cell applies to the usage rows it reads at the end of the scenario
/// ([`Cell::set_usage_fault`], [`UsageReadFault`]).
#[derive(Clone, Debug)]
pub struct SetUsageFault(UsageReadFault);

/// Applies `fault` to the usage rows the cell reads from now on.
#[must_use]
pub fn set_usage_fault(fault: UsageReadFault) -> SetUsageFault {
    SetUsageFault(fault)
}

impl Action for SetUsageFault {
    fn run(&self, cell: &mut Cell) {
        cell.set_usage_fault(self.0);
    }
}
