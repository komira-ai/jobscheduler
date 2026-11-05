//! Starts new agent processes.

use super::{Action, Agents};
use crate::cell::Cell;

/// Starts a new process, with empty state, on each agent's host ([`Cell::replace`]); a
/// killed process's fault ends.
#[derive(Clone, Debug)]
pub struct ReplaceAgents(Agents);

/// Starts new processes on the hosts of `agents`.
#[must_use]
pub fn replace_agents(agents: Agents) -> ReplaceAgents {
    ReplaceAgents(agents)
}

impl Action for ReplaceAgents {
    fn run(&self, cell: &mut Cell) {
        for index in self.0.pick(cell) {
            cell.replace(index);
        }
    }
}
