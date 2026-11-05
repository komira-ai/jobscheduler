//! Changes the faults of the agent wire.

use js_sim::Faults;

use super::Action;
use crate::cell::Cell;

/// Replaces the bus's faults ([`Cell::set_faults`], through [`js_sim::Sim::set_faults`]):
/// a message sent from now on takes them. A drop, duplicate or reorder chance above none
/// is a fault until faults without one are set. The client's calls never take them.
#[derive(Clone, Debug)]
pub struct SetBusFaults(Faults);

/// Puts `faults` on the bus now.
#[must_use]
pub fn set_bus_faults(faults: Faults) -> SetBusFaults {
    SetBusFaults(faults)
}

impl Action for SetBusFaults {
    fn run(&self, cell: &mut Cell) {
        cell.set_faults(self.0.clone());
    }
}
