//! The drain cost of a worker, for the worker a blocked whole-machine request holds
//! ([`Scheduler::with_whole_node`]).

use std::collections::BTreeMap;
use std::time::Duration;

use js_types::sched::{OperationId, WorkerId};
use js_whole::cost::{DrainCost, HostLoad, drain_cost};

use crate::scheduler::Scheduler;
use crate::servable::Placed;

impl Scheduler {
    /// Names the longest `operation` may run once leased, which the caller knows from the
    /// request's wall time: no scheduler input and no log record carries it. A lease's end
    /// bound is its start plus this; a lease whose operation has none is taken to end at
    /// the latest time there is, so a worker holding it drains last. Set it with the
    /// submit and again for an operation taken up after a restart (a recovered lease's
    /// start is the recovery, so its bound is that plus this). Dropped with the operation.
    pub fn set_max_wall(&mut self, operation: OperationId, max_wall: Duration) {
        self.max_walls.insert(operation, max_wall);
    }

    /// The registered workers by drain cost, cheapest first, when whole-node settings are
    /// on; `None` when they are not. `placed` are this round's placements, not yet held.
    pub(super) fn drain_order(&self, placed: &[(OperationId, Placed)]) -> Option<Vec<DrainCost>> {
        self.whole_node.as_ref()?;
        let end_of = |operation: OperationId, start: u64| {
            self.max_walls.get(&operation).map_or(u64::MAX, |wall| {
                start.saturating_add(u64::try_from(wall.as_millis()).unwrap_or(u64::MAX))
            })
        };
        // Every held lease belongs to an operation that holds it.
        let mut ends: BTreeMap<&WorkerId, Vec<u64>> = BTreeMap::new();
        for held in self.held.values() {
            let start = held.started_at.unwrap_or(held.granted_at).as_millis();
            let end = end_of(held.operation, start);
            let holding = self.ops[&held.operation].state.holding();
            holding
                .into_iter()
                .for_each(|(_, worker)| ends.entry(worker).or_default().push(end));
        }
        for (operation, placed) in placed {
            let end = end_of(*operation, self.now.as_millis());
            ends.entry(&placed.worker).or_default().push(end);
        }
        let loads: Vec<HostLoad> = self
            .workers
            .iter()
            .map(|(name, w)| HostLoad {
                id: name.as_str().to_owned(),
                leases: w.leases,
                end_bounds: ends.remove(name).unwrap_or_default(),
            })
            .collect();
        Some(drain_cost(&loads, self.now.as_millis()))
    }
}
