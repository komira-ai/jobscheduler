//! Adds a machine the scheduler did not launch to the elastic pool's cloud.

use super::Action;
use crate::cell::Cell;

/// Adds a running machine to the cloud's account that no acquire of the scheduler launched,
/// another scheduler's under a name or an unmarked one with none, on the pool's first offer
/// ([`Cell::add_foreign_machine`]). A list never shows it and a release refuses it, and it
/// counts toward the cloud's quota and room. Nothing for a cell without an elastic pool.
#[derive(Clone, Debug)]
pub struct AddForeignMachine(Option<String>);

/// Adds a foreign machine named `name` (or unnamed) now.
#[must_use]
pub fn add_foreign_machine(name: Option<&str>) -> AddForeignMachine {
    AddForeignMachine(name.map(str::to_owned))
}

impl Action for AddForeignMachine {
    fn run(&self, cell: &mut Cell) {
        cell.add_foreign_machine(self.0.as_deref());
    }
}
