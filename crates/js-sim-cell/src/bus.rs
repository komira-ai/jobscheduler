//! What the cell's agents and its server send each other: one HTTP call, as a request
//! and its answer ([`Msg`]).
//!
//! The kernel's bus ([`js_sim::Faults`]) can drop, delay, duplicate and reorder a
//! message. This bus models HTTP, which carries a request and its answer once each and
//! in order on one connection, so the cell runs with duplication and reordering off:
//! [`js_sim::Faults::default`] leaves both at `never()`, and the cell keeps them there. A
//! dropped message is a connection reset (the request or its answer is lost), a delay is
//! a slow network, and a [`js_sim::Partition`] is a cut link. A scenario that models a
//! request retried over a second connection, which can overtake the first, turns
//! `reorder` on for itself with [`js_sim::Sim::set_faults`].
//!
//! A message carries its body as the JSON text the HTTP route carries, built from and
//! read into the types of `js-agent-wire`, so the agent and the server read exactly the
//! bytes a real peer would send. Its trace line ([`trace_msg`]) holds the route, the
//! call id and the status only: never the body or the bearer, so no join token, session
//! secret or random id reaches a trace.

use std::time::Duration;

use js_sim::NodeId;

/// The name of the cell's server node.
pub const SERVER: &str = "server";

/// The name of the cell's agent number `index`, counted from zero in the order the
/// agents are added: `h000`, `h001`, and so on.
#[must_use]
pub fn agent_name(index: usize) -> NodeId {
    NodeId::new(format!("h{index:03}"))
}

/// One call's number, chosen by the caller and repeated in the answer. An agent numbers
/// its calls from zero for the life of its host, across its processes, so an answer to a
/// call of a process that died never reads as an answer to its replacement's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallId(pub u64);

/// The agent routes a call can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Route {
    /// `POST /v1/agent/hello`: a [`js_agent_wire::Hello`], answered by a
    /// [`js_agent_wire::Welcome`] or a refusal.
    Hello,
    /// `POST /v1/agent/heartbeat`: a [`js_agent_wire::Heartbeat`], answered by a
    /// [`js_agent_wire::HeartbeatReply`] or a refusal.
    Heartbeat,
}

impl Route {
    /// The route's path.
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Route::Hello => "/v1/agent/hello",
            Route::Heartbeat => "/v1/agent/heartbeat",
        }
    }
}

/// A message between an agent and the server.
///
/// It has no `Debug`: the bearer and the body hold secrets. [`trace_msg`] is how a
/// message is written.
#[derive(Clone, PartialEq, Eq)]
pub enum Msg {
    /// A call, agent to server.
    Request {
        /// The call's number.
        id: CallId,
        /// The route called.
        route: Route,
        /// The bearer token of the `Authorization` header.
        bearer: String,
        /// The JSON body.
        body: String,
    },
    /// The answer to the call `id`, server to agent.
    Reply {
        /// The call answered.
        id: CallId,
        /// The HTTP status.
        status: u16,
        /// The `Retry-After` header, when the answer has one: the one header an agent
        /// reads.
        retry_after: Option<Duration>,
        /// The JSON body: the route's answer on a 200, the API's `{"error": "<code>"}`
        /// otherwise.
        body: String,
    },
}

/// `msg` as a trace line spells it: `call #<id> <path>` or `answer #<id> <status>`. The
/// bearer, the body and the `Retry-After` value are left out.
#[must_use]
pub fn trace_msg(msg: &Msg) -> String {
    match msg {
        Msg::Request { id, route, .. } => format!("call #{} {}", id.0, route.path()),
        Msg::Reply { id, status, .. } => format!("answer #{} {status}", id.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: agents named by anything but their index in three digits, from zero, which
    /// would give two agents one name or break the order the cell adds them in.
    #[test]
    fn agents_are_named_by_their_index_in_three_digits() {
        assert_eq!(agent_name(0), NodeId::new("h000"));
        assert_eq!(agent_name(1), NodeId::new("h001"));
        assert_eq!(agent_name(42), NodeId::new("h042"));
        assert_eq!(agent_name(999), NodeId::new("h999"));
        assert!(agent_name(9) < agent_name(10));
    }
}
