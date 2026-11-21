//! Holds agents' hosts as the drain of an update does.

use super::{Action, Agents};
use crate::cell::Cell;

/// Holds the hosts of the agents ([`Cell::hold`]): a real agent writes its hold file and
/// refuses every start from then on with the code `host-held`, making nothing. It does not
/// cordon the host, which a held agent's own keeper does after its hello; a scenario that
/// wants the cordon too cordons the host itself (`cordon_host`).
#[derive(Clone, Debug)]
pub struct HoldAgents {
    agents: Agents,
}

/// Holds the hosts of `agents`, from now.
#[must_use]
pub fn hold_agents(agents: Agents) -> HoldAgents {
    HoldAgents { agents }
}

impl Action for HoldAgents {
    fn run(&self, cell: &mut Cell) {
        for index in self.agents.pick(cell) {
            cell.hold(index);
        }
    }
}
