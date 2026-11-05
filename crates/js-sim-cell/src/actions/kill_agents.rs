//! Kills agent processes.

use super::{Action, Agents};
use crate::cell::Cell;

/// Kills the process of each agent: it holds nothing and drops every input until it is
/// replaced ([`Cell::kill`]). A killed process is a fault until it is replaced.
#[derive(Clone, Debug)]
pub struct KillAgents(Agents);

/// Kills the processes of `agents`.
#[must_use]
pub fn kill_agents(agents: Agents) -> KillAgents {
    KillAgents(agents)
}

impl Action for KillAgents {
    fn run(&self, cell: &mut Cell) {
        for index in self.0.pick(cell) {
            cell.kill(index);
        }
    }
}
