//! Kills agent processes for good.

use super::{Action, Agents};
use crate::cell::Cell;

/// Kills the process of each agent and never replaces it ([`Cell::kill_for_good`]): the
/// host is gone, not faulted, so a scenario with such hosts can settle while they stay
/// silent.
#[derive(Clone, Debug)]
pub struct KillAgentsForGood(Agents);

/// Kills the processes of `agents` for good.
#[must_use]
pub fn kill_agents_for_good(agents: Agents) -> KillAgentsForGood {
    KillAgentsForGood(agents)
}

impl Action for KillAgentsForGood {
    fn run(&self, cell: &mut Cell) {
        for index in self.0.pick(cell) {
            cell.kill_for_good(index);
        }
    }
}
