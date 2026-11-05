//! Makes agents take a while to prepare a whole host.

use std::time::Duration;

use super::{Action, Agents};
use crate::agent::Prepare;
use crate::cell::Cell;

/// Makes the agents report ready for a whole-host lease only once `delay` has passed
/// since they were told of it, and no run is held ([`Prepare::Ready`]), from their next
/// heartbeat on.
#[derive(Clone, Debug)]
pub struct AgentPrepareDelay {
    agents: Agents,
    delay: Duration,
}

/// Makes `agents` take `delay` to prepare, from now.
#[must_use]
pub fn agent_prepare_delay(agents: Agents, delay: Duration) -> AgentPrepareDelay {
    AgentPrepareDelay { agents, delay }
}

impl Action for AgentPrepareDelay {
    fn run(&self, cell: &mut Cell) {
        let prepare = Prepare::Ready { delay: self.delay };
        for index in self.agents.pick(cell) {
            cell.set_prepare(index, prepare);
        }
    }
}
