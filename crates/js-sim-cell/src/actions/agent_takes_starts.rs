//! Makes agents that refuse job starts take them again.

use super::{Action, Agents};
use crate::cell::Cell;

/// Makes the agents take every job start they are told of, as they did before
/// [`agent_refuses_starts`](super::agent_refuses_starts).
#[derive(Clone, Debug)]
pub struct AgentTakesStarts {
    agents: Agents,
}

/// Makes `agents` take job starts again, from now. A result already made for a refused
/// start stays until the server acknowledges it.
#[must_use]
pub fn agent_takes_starts(agents: Agents) -> AgentTakesStarts {
    AgentTakesStarts { agents }
}

impl Action for AgentTakesStarts {
    fn run(&self, cell: &mut Cell) {
        for index in self.agents.pick(cell) {
            cell.set_start_refusal(index, None);
        }
    }
}
