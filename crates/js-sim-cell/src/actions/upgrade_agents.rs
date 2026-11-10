//! Upgrades agents from the previous release to this one.

use super::{Action, Agents};
use crate::cell::Cell;

/// Makes each agent an agent of the current release ([`Cell::upgrade`]): its next hello is
/// the current release's, and a new process, with empty state, takes the place of the one
/// it had, as an upgrade restarts the agent. An agent that is already current is only
/// restarted.
#[derive(Clone, Debug)]
pub struct UpgradeAgents(Agents);

/// Upgrades the hosts of `agents`.
#[must_use]
pub fn upgrade_agents(agents: Agents) -> UpgradeAgents {
    UpgradeAgents(agents)
}

impl Action for UpgradeAgents {
    fn run(&self, cell: &mut Cell) {
        for index in self.0.pick(cell) {
            cell.upgrade(index);
        }
    }
}
