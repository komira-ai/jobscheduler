//! What a scenario does to the cell at a virtual time: one [`Action`] per file, each a
//! value the scenario builder holds and runs once per seed.
//!
//! An action takes the [`Cell`] at its virtual time and nothing else. A choice it makes
//! (which agents) is a literal, or drawn from the cell's random source ([`Cell::rng`]),
//! so it follows the seed. A later part adds an action by adding a file here and one
//! `pub use` line below.

use std::ops::Range;

use crate::cell::Cell;
use crate::observe::ItemId;

mod agent_prepare_delay;
mod agent_refuses_prepare;
mod agent_silent_on_prepare;
mod cancel_work;
mod kill_agents;
mod kill_agents_for_good;
mod partition_agents;
mod replace_agents;
mod request_whole_host;
mod restart_server;
mod set_bus_faults;
mod submit_work;

pub use agent_prepare_delay::{AgentPrepareDelay, agent_prepare_delay};
pub use agent_refuses_prepare::{AgentRefusesPrepare, agent_refuses_prepare};
pub use agent_silent_on_prepare::{AgentSilentOnPrepare, agent_silent_on_prepare};
pub use cancel_work::{CancelWork, cancel_work};
pub use kill_agents::{KillAgents, kill_agents};
pub use kill_agents_for_good::{KillAgentsForGood, kill_agents_for_good};
pub use partition_agents::{PartitionAgents, partition_agents};
pub use replace_agents::{ReplaceAgents, replace_agents};
pub use request_whole_host::{RequestWholeHost, request_whole_host};
pub use restart_server::{RestartServer, restart_server};
pub use set_bus_faults::{SetBusFaults, set_bus_faults};
pub use submit_work::{SubmitWork, submit_work};

/// A change a scenario makes to the cell at a virtual time. The builder runs it once in
/// each seed's cell, at the time it was scripted for.
pub trait Action {
    /// Makes the change, now.
    fn run(&self, cell: &mut Cell);
}

/// Which agents an action acts on, by the index they were added at (`h000` is 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Agents {
    /// These agents.
    Listed(Vec<usize>),
    /// This many agents, drawn from the cell's random source when the action runs; all of
    /// them when the cell has no more.
    Drawn(usize),
}

impl Agents {
    /// The agents of the action, in index order.
    #[must_use]
    pub fn pick(&self, cell: &mut Cell) -> Vec<usize> {
        let mut picked = match self {
            Agents::Listed(listed) => listed.clone(),
            Agents::Drawn(count) => {
                let mut all: Vec<usize> = (0..cell.agent_count()).collect();
                cell.rng().shuffle(&mut all);
                all.truncate(*count);
                all
            }
        };
        picked.sort_unstable();
        picked
    }
}

/// The agents of `range`, by index.
#[must_use]
pub fn agents(range: Range<usize>) -> Agents {
    Agents::Listed(range.collect())
}

/// `count` agents drawn from the seed when the action runs.
#[must_use]
pub fn drawn_agents(count: usize) -> Agents {
    Agents::Drawn(count)
}

/// The items of `range`, by number.
#[must_use]
pub fn items(range: Range<u32>) -> Vec<ItemId> {
    range.map(ItemId).collect()
}
