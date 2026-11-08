//! Kills agent processes that run jobs, and starts new ones on their hosts later.

use std::time::Duration;

use super::Action;
use crate::agent::AgentNode;
use crate::cell::Cell;

/// Kills the process of each of the first agents, in index order, that runs at least one
/// job and runs nothing its heartbeats have not listed ([`AgentNode::runs_unlisted`]), up
/// to a count ([`Cell::kill`]), and starts a new process with empty state on each of their
/// hosts a while later ([`Cell::replace`]). The agents are chosen when the action runs, from
/// what their processes run then, so each is killed mid-job; and since the server has seen
/// every job of theirs listed, the new process's first listing, which lists none of them,
/// is the host's word that they are gone. A cell with fewer such agents has all of them
/// killed. A killed process is a fault until it is replaced.
#[derive(Clone, Debug)]
pub struct KillAgentsMidJob {
    count: usize,
    replace_after: Duration,
}

/// Kills up to `count` agents running listed jobs now, and replaces them `replace_after`
/// later.
#[must_use]
pub fn kill_agents_mid_job(count: usize, replace_after: Duration) -> KillAgentsMidJob {
    KillAgentsMidJob {
        count,
        replace_after,
    }
}

/// Whether `agent`'s process runs a job, and runs nothing its heartbeats have not listed.
fn mid_job(agent: &AgentNode) -> bool {
    let jobs = agent.truth().is_some_and(|truth| !truth.jobs.is_empty());
    jobs & !agent.runs_unlisted()
}

impl Action for KillAgentsMidJob {
    fn run(&self, cell: &mut Cell) {
        let all = 0..cell.agent_count();
        let busy = all.filter(|index| cell.agent(*index).is_some_and(mid_job));
        let picked: Vec<usize> = busy.take(self.count).collect();
        for index in &picked {
            cell.kill(*index);
        }
        let at = cell.now().saturating_add(self.replace_after);
        cell.at(at, move |cell| {
            for index in picked {
                cell.replace(index);
            }
        });
    }
}
