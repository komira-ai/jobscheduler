//! The kinds of node a cell runs: its server and its agents.

use std::cell::RefCell;
use std::rc::Rc;

use js_sim::{Node, NodeInput, Output};
use js_types::HostId;
use js_types::sched::{Effect, StateMachine};

use crate::agent::AgentNode;
use crate::bus::{self, Msg};
use crate::server::ServerNode;

/// A node of the cell: the server, or the host of one agent.
pub enum CellNode {
    /// The real lease server.
    Server(Box<ServerNode>),
    /// One host and its agent process.
    Agent(Box<AgentNode>),
}

impl CellNode {
    /// The server, when this node is it.
    #[must_use]
    pub fn as_server(&self) -> Option<&ServerNode> {
        match self {
            CellNode::Server(server) => Some(server),
            CellNode::Agent(_) => None,
        }
    }

    /// The server, when this node is it, to call.
    pub fn as_server_mut(&mut self) -> Option<&mut ServerNode> {
        match self {
            CellNode::Server(server) => Some(server),
            CellNode::Agent(_) => None,
        }
    }

    /// The agent's host, when this node is one.
    #[must_use]
    pub fn as_agent(&self) -> Option<&AgentNode> {
        match self {
            CellNode::Agent(agent) => Some(agent),
            CellNode::Server(_) => None,
        }
    }

    /// The agent's host, when this node is one, to kill or replace its process.
    pub fn as_agent_mut(&mut self) -> Option<&mut AgentNode> {
        match self {
            CellNode::Agent(agent) => Some(agent),
            CellNode::Server(_) => None,
        }
    }
}

impl StateMachine for CellNode {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match self {
            CellNode::Server(server) => server.apply(input),
            CellNode::Agent(agent) => agent.apply(input),
        }
    }
}

impl Node for CellNode {
    type Msg = Msg;

    fn trace_msg(msg: &Msg) -> String {
        bus::trace_msg(msg)
    }

    fn may_overtake(msg: &Msg) -> bool {
        bus::may_overtake(msg)
    }

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            CellNode::Server(server) => server.take_outputs(),
            CellNode::Agent(agent) => agent.take_outputs(),
        }
    }
}

/// The host of the last agent that took an input, shared between the agents and the
/// cell's run loop: the kernel does not say which node a step fed, and a step check reads
/// only the agent it touched.
#[derive(Clone, Debug, Default)]
pub(crate) struct Applied(Rc<RefCell<Option<HostId>>>);

impl Applied {
    /// The agent of `host` took an input.
    pub(crate) fn agent(&self, host: &HostId) {
        *self.0.borrow_mut() = Some(host.clone());
    }

    /// The agent that took an input since the last call, if one did.
    pub(crate) fn take(&self) -> Option<HostId> {
        self.0.borrow_mut().take()
    }
}
