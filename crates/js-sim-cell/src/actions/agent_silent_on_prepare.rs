//! Makes agents never answer a prepare.

use super::{Action, Agents};
use crate::agent::Prepare;
use crate::cell::Cell;

/// Makes the agents stay preparing for every whole-host lease they are told of, never
/// reporting ready or refusing ([`Prepare::Silent`]), from their next heartbeat on: the
/// server's prepare deadline is what ends such a lease.
#[derive(Clone, Debug)]
pub struct AgentSilentOnPrepare {
    agents: Agents,
}

/// Makes `agents` silent on a prepare, from now.
#[must_use]
pub fn agent_silent_on_prepare(agents: Agents) -> AgentSilentOnPrepare {
    AgentSilentOnPrepare { agents }
}

impl Action for AgentSilentOnPrepare {
    fn run(&self, cell: &mut Cell) {
        for index in self.agents.pick(cell) {
            cell.set_prepare(index, Prepare::Silent);
        }
    }
}
