//! Cuts agents off from the server for a while.

use std::time::Duration;

use super::{Action, Agents};
use crate::cell::Cell;

/// Cuts the agents off from the server and from the other agents, as one group, in
/// place of any partition in force ([`Cell::partition`]), and heals every partition
/// `lasting` later ([`Cell::heal`]).
#[derive(Clone, Debug)]
pub struct PartitionAgents {
    agents: Agents,
    lasting: Duration,
}

/// Cuts `agents` off now and heals the cut `lasting` later.
#[must_use]
pub fn partition_agents(agents: Agents, lasting: Duration) -> PartitionAgents {
    PartitionAgents { agents, lasting }
}

impl Action for PartitionAgents {
    fn run(&self, cell: &mut Cell) {
        let group = self.agents.pick(cell);
        cell.partition(&[group.as_slice()]);
        let heal_at = cell.now().saturating_add(self.lasting);
        cell.at(heal_at, Cell::heal);
    }
}
