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

mod add_foreign_machine;
mod cancel_work;
mod cordon_host;
mod crash_at_next_acquire;
mod drop_machine;
mod join_agents;
mod kill_agents;
mod kill_agents_for_good;
mod kill_agents_mid_job;
mod lose_submit_answer;
mod machines_never_join;
mod mint_join_token;
mod partition_agents;
mod preempt_machine;
mod replace_agents;
mod reset_brake;
mod restart_server;
mod script_provider;
mod script_provider_forever;
mod set_bus_faults;
mod set_knobs;
mod set_usage_fault;
mod start_second_server;
mod stop_server;
mod submit_work;
mod uncordon_host;

pub use add_foreign_machine::{AddForeignMachine, add_foreign_machine};
pub use cancel_work::{CancelWork, cancel_work};
pub use cordon_host::{CordonHost, cordon_host};
pub use crash_at_next_acquire::{CrashAtNextAcquire, crash_at_next_acquire};
pub use drop_machine::{DropMachine, drop_machine};
pub use join_agents::{JoinAgents, join_agents};
pub use kill_agents::{KillAgents, kill_agents};
pub use kill_agents_for_good::{KillAgentsForGood, kill_agents_for_good};
pub use kill_agents_mid_job::{KillAgentsMidJob, kill_agents_mid_job};
pub use lose_submit_answer::{LoseSubmitAnswer, lose_submit_answer};
pub use machines_never_join::{MachinesNeverJoin, machines_never_join};
pub use mint_join_token::{MintJoinToken, mint_join_token};
pub use partition_agents::{PartitionAgents, partition_agents};
pub use preempt_machine::{PreemptMachine, preempt_machine};
pub use replace_agents::{ReplaceAgents, replace_agents};
pub use reset_brake::{ResetBrake, reset_brake};
pub use restart_server::{RestartServer, restart_server};
pub use script_provider::{ScriptProvider, script_provider};
pub use script_provider_forever::{ScriptProviderForever, script_provider_forever};
pub use set_bus_faults::{SetBusFaults, set_bus_faults};
pub use set_knobs::{SetKnobs, set_knobs};
pub use set_usage_fault::{SetUsageFault, set_usage_fault};
pub use start_second_server::{StartSecondServer, start_second_server};
pub use stop_server::{StopServer, stop_server};
pub use submit_work::{SubmitWork, submit_work};
pub use uncordon_host::{UncordonHost, uncordon_host};

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
