//! Cuts agents off from the server for a while.

use std::time::Duration;

use super::{Action, Agents};
use crate::cell::Cell;

/// Cuts the agents off from the server and from the other agents, as one group, in
/// place of any partition in force ([`Cell::partition`]), and heals every partition
/// `lasting` later ([`Cell::heal`]). At the cut, each agent that runs a run no hello or
/// heartbeat of its own listed, so the server cannot have seen it listed, counts one
/// [`crate::Counter::CutWithUnlistedRun`]: the witness of a scenario about a host that
/// goes silent while it holds a run the server never saw listed (design section 8, row
/// 35).
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
        for index in &group {
            cell.note_cut(*index);
        }
        cell.partition(&[group.as_slice()]);
        let heal_at = cell.now().saturating_add(self.lasting);
        cell.at(heal_at, Cell::heal);
    }
}
