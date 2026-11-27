//! Starts hosts that say hello with host certificates.

use super::Action;
use crate::cell::Cell;

/// What a started host presents.
#[derive(Clone, Debug)]
enum Presents {
    /// Its own host certificate.
    Own(usize),
    /// The host certificate of the agent with this index.
    Of(usize),
}

/// Adds hosts that say hello with a host certificate: their own, or another host's
/// ([`Cell::join_presenting`]).
#[derive(Clone, Debug)]
pub struct JoinAgents(Presents);

/// Adds `count` hosts now, each saying hello with the host certificate of its own host.
#[must_use]
pub fn join_agents(count: usize) -> JoinAgents {
    JoinAgents(Presents::Own(count))
}

/// Adds one host now that says hello with the host certificate of the agent of index `of`:
/// the server refuses it, its certificate naming another host than its hello does.
#[must_use]
pub fn join_agent_presenting(of: usize) -> JoinAgents {
    JoinAgents(Presents::Of(of))
}

impl Action for JoinAgents {
    fn run(&self, cell: &mut Cell) {
        match self.0 {
            Presents::Own(count) => {
                for _ in 0..count {
                    // A host id of a thousand million agents is the only refusal.
                    let _added = cell.add_agent();
                }
            }
            Presents::Of(of) => {
                cell.join_presenting(of);
            }
        }
    }
}
