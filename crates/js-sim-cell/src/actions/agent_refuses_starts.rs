//! Makes agents refuse the job starts they are told of.

use super::{Action, Agents};
use crate::cell::Cell;

/// A refusal code that is the host's, not the job's: the agent's marker is unwritable
/// ([`js_agent_wire::refusal::HOST_TAINTED`]). The server ends the attempt as an agent
/// refusal that a retry policy retries on another host, and does not requeue it.
pub const HOST_REFUSAL: &str = js_agent_wire::refusal::HOST_TAINTED;

/// Makes the agents answer every job start with a refusal carrying `code`: the job
/// launches nothing, and the result is kept until the server acknowledges it.
#[derive(Clone, Debug)]
pub struct AgentRefusesStarts {
    agents: Agents,
    code: String,
}

/// Makes `agents` refuse every job start with `code`, from now, until
/// [`agent_takes_starts`](super::agent_takes_starts). Use [`HOST_REFUSAL`] for a code that
/// is the host's and retries. Only fake per-job agents refuse; a real agent takes the
/// start.
#[must_use]
pub fn agent_refuses_starts(agents: Agents, code: &str) -> AgentRefusesStarts {
    AgentRefusesStarts {
        agents,
        code: code.to_owned(),
    }
}

impl Action for AgentRefusesStarts {
    fn run(&self, cell: &mut Cell) {
        for index in self.agents.pick(cell) {
            cell.set_start_refusal(index, Some(self.code.clone()));
        }
    }
}
