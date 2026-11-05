//! Makes agents refuse to prepare a whole host.

use js_agent_wire::RefusalReason;

use super::{Action, Agents};
use crate::agent::Prepare;
use crate::cell::Cell;

/// Makes the agents refuse every whole-host lease they are told to prepare for, with
/// `reason` ([`Prepare::Refuse`]), from their next heartbeat on.
#[derive(Clone, Debug)]
pub struct AgentRefusesPrepare {
    agents: Agents,
    reason: RefusalReason,
}

/// Makes `agents` refuse to prepare with `reason`, from now.
#[must_use]
pub fn agent_refuses_prepare(agents: Agents, reason: RefusalReason) -> AgentRefusesPrepare {
    AgentRefusesPrepare { agents, reason }
}

impl Action for AgentRefusesPrepare {
    fn run(&self, cell: &mut Cell) {
        for index in self.agents.pick(cell) {
            cell.set_prepare(index, Prepare::Refuse(self.reason));
        }
    }
}
