//! The reference placement round of the F3 checker: in queue order, each operation to the
//! least loaded worker with room (the booked rule) or where the load reference puts it
//! (placement by load). A module of the checker, for its private state.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use js_sched::PLACEMENT_ROUND;
use js_types::sched::{OperationId, Resources, WorkerId};

use super::{Check, Class, Round, TERM, by_load, exclusive, room};
use crate::load_reference::{Ask, Host, Placer};

impl Check {
    /// The reference round at `now`: in queue order, each operation to the least loaded
    /// worker with room.
    pub(super) fn round(&mut self) -> Round {
        // Per worker: what its holdings book, how many there are, and whether one holds
        // the node alone.
        let mut booked: BTreeMap<WorkerId, (Resources, usize, bool)> = BTreeMap::new();
        let mut queue: Vec<OperationId> = self
            .ops
            .iter()
            .filter(|(id, op)| !op.done && !op.refusing && !self.holding.contains_key(id))
            .map(|(id, _)| *id)
            .collect();
        for (id, h) in &self.holding {
            let b = booked.entry(h.worker.clone()).or_default();
            *b = (
                b.0.saturating_add(self.ops[id].request.resources),
                b.1 + 1,
                b.2 || h.alone,
            );
        }
        queue.sort_by_key(|id| (Reverse(self.ops[id].request.qos.clone()), *id));
        let names: Vec<WorkerId> = self.workers.keys().cloned().collect();
        let mut round = Round::default();
        let mut placer = self.load.then(|| {
            let hosts = names.iter().map(|n| self.host(n)).collect();
            let next_seq = self.newest_grant.map_or(0, |l| l.seq + 1);
            Placer::new(hosts, TERM, next_seq, self.holds.clone())
        });
        // The platform requests seen this round: a request's pool is the one its platform is.
        let mut pools: Vec<js_caps::Request> = Vec::new();
        for id in queue {
            let request = &self.ops[&id].request;
            let usable = |w: &WorkerId, cordoned: bool| {
                let s = &self.workers[w];
                self.live(w)
                    && self.cordons.contains_key(w) == cordoned
                    && request.needs.matches(&s.caps)
            };
            // Placement skips the workers of `avoid` while another could run it.
            let elsewhere = names.iter().any(|w| {
                usable(w, false)
                    && !self.ops[&id].avoid.contains(w)
                    && self.workers[w].capacity.fits(&request.resources)
            });
            let fit = if let Some(placer) = placer.as_mut() {
                // The load rule: the reference placement of `load_reference`.
                let pool = pools
                    .iter()
                    .position(|p| *p == request.needs)
                    .unwrap_or_else(|| {
                        pools.push(request.needs.clone());
                        pools.len() - 1
                    });
                let usable: Vec<usize> = (0..names.len())
                    .filter(|&i| {
                        usable(&names[i], false)
                            && !(elsewhere && self.ops[&id].avoid.contains(&names[i]))
                    })
                    .collect();
                let ask = Ask {
                    operation: id,
                    key: &request.key,
                    resources: request.resources,
                    pool,
                    candidates: &usable,
                };
                if round.grants.len() < PLACEMENT_ROUND {
                    placer.place(&ask).map(|i| &names[i])
                } else {
                    None
                }
            } else {
                // The least loaded worker with room; of equals, the first by name.
                let load = |w: &WorkerId| {
                    let (held, leases, _) = booked.get(w).copied().unwrap_or_default();
                    (self.workers[w].capacity, held, leases)
                };
                names
                    .iter()
                    .filter(|w| {
                        let (held, leases, whole) = booked.get(*w).copied().unwrap_or_default();
                        usable(w, false)
                            && room(
                                self.workers[*w].capacity,
                                held,
                                whole,
                                leases,
                                request.resources,
                            )
                            && !(elsewhere && self.ops[&id].avoid.contains(*w))
                    })
                    .min_by(|a, b| by_load(load(a), load(b)))
                    .filter(|_| round.grants.len() < PLACEMENT_ROUND)
            };
            let class = match fit {
                Some(w) => {
                    let alone = exclusive(self.workers[w].capacity, request.resources);
                    let b = booked.entry(w.clone()).or_default();
                    *b = (b.0.saturating_add(request.resources), b.1 + 1, b.2 || alone);
                    round.grants.push((id, w.clone()));
                    Class::Servable
                }
                None => {
                    let large = |w: &&WorkerId| self.workers[*w].capacity.fits(&request.resources);
                    if names.iter().filter(|w| usable(w, false)).any(|w| large(&w)) {
                        Class::Servable
                    } else if names.iter().filter(|w| usable(w, true)).any(|w| large(&w)) {
                        Class::Cordoned
                    } else {
                        Class::Unservable
                    }
                }
            };
            let op = self.ops.get_mut(&id).expect("queued ops exist");
            op.since = match (class, op.last_class) {
                (Class::Unservable, Some(Class::Unservable)) => op.since,
                (Class::Unservable, _) => Some(self.now),
                _ => None,
            };
            op.last_class = Some(class);
            if op.since.is_some_and(|s| self.now >= s + self.wait_ms) {
                round.refusals.insert(id);
            }
            round.classes.insert(id, class);
        }
        if let Some(placer) = placer {
            self.holds = placer.holds().clone();
        }
        round
    }

    /// Worker `name` as the load reference meets it: its capacity, the GPUs and VM slots its
    /// holdings book, whether one holds it alone, and what each was charged.
    pub(super) fn host(&self, name: &WorkerId) -> Host {
        let booked = self.booked_on(name);
        let (_, whole) = self.holds_on(name);
        let charged = self
            .holding
            .iter()
            .filter(|(_, h)| h.worker == *name)
            .map(|(id, h)| (self.ops[id].request.key.clone(), h.cores))
            .collect();
        Host::new(
            name.clone(),
            self.workers[name].capacity,
            booked.gpus,
            booked.vms,
            whole,
            charged,
        )
    }
}
