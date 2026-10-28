//! One placement round, checked against a reference placement (each operation to the
//! least loaded worker with room) and the reference unservable verdict: I1, I6, I7 and
//! I8 for each grant, I10 and L2 for waiting reasons and refusals, I11 for the round.

use std::collections::BTreeMap;

use js_types::sched::{ControlRecord, Effect, LeaseId, OperationId, Resources, WorkerId};

use crate::load_reference::{Ask, Host, Placer};

use std::cmp::Ordering;

use super::{
    Checker, Held, Room, State, TERM, add, by_load, exclusive, fits_beside, fits_whole, queue_key,
};

/// At most this many grants per round: the catalog's number, written here rather than
/// read from `js_sched::PLACEMENT_ROUND`, so that a change to the constant is caught.
pub const ROUND: usize = 256;

/// Where a queued operation stands in the reference verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Servable,
    /// No live worker, cordoned or not, could run it: the wait counts.
    Unservable,
    /// Only cordoned live workers could: it waits and is never refused.
    Cordoned,
}

impl Checker {
    /// Worker `name` as the load reference meets it: its capacity, the GPUs it books, whether
    /// one lease holds it alone, and what each lease was charged.
    pub(super) fn host(&self, name: &WorkerId) -> Host {
        let w = &self.workers[name];
        let capacity = Resources::new(w.capacity[0], w.capacity[1]).with_gpus(w.capacity[2]);
        let charged = w.charged.values().cloned().collect();
        Host::new(name.clone(), capacity, w.booked[2], 0, w.whole, charged)
    }

    /// The reference placement of the load rule over `names`, the workers placement may use.
    fn placer(&self, names: &[WorkerId]) -> Placer {
        let hosts = names.iter().map(|n| self.host(n)).collect();
        let next_seq = self.last_lease.map_or(0, |l| l.seq + 1);
        Placer::new(hosts, TERM, next_seq, self.holds.clone())
    }

    /// The reference verdict on queued operation `id`.
    fn verdict(&mut self, id: OperationId) -> Verdict {
        let (need, res) = {
            let op = &self.ops[&id];
            (op.need, op.resources)
        };
        let names: Vec<WorkerId> = self.workers.keys().cloned().collect();
        let mut cordoned_could = false;
        for name in names {
            if !self.live(&name) || !fits_whole(self.workers[&name].capacity, res) {
                continue;
            }
            if !self.matches(need, &name) {
                continue;
            }
            if self.cordoned.contains(&name) {
                cordoned_could = true;
            } else {
                return Verdict::Servable;
            }
        }
        if cordoned_could {
            Verdict::Cordoned
        } else {
            Verdict::Unservable
        }
    }

    /// One placement round (a tick after its expiry, or an uncordon): the grants must
    /// be the reference placement's, in order; waiting reasons and refusals must agree
    /// with the reference verdict and the unservable wait.
    pub(super) fn round(&mut self, effects: &[Effect]) {
        self.stats.rounds += 1;
        let order: Vec<OperationId> = self.queue.iter().map(|(_, _, id)| *id).collect();
        let names: Vec<WorkerId> = self
            .workers
            .keys()
            .filter(|n| self.live(n) && !self.cordoned.contains(*n))
            .cloned()
            .collect();
        let mut booked: BTreeMap<WorkerId, Room> = names
            .iter()
            .map(|n| {
                let w = &self.workers[n];
                let room = Room {
                    booked: w.booked,
                    leases: w.leases,
                    whole: w.whole,
                };
                (n.clone(), room)
            })
            .collect();
        let mut placer = self.load.then(|| self.placer(&names));
        let mut expected = Vec::new();
        let mut verdicts = Vec::new();
        let mut left_over = false;
        for &id in &order {
            let (need, res) = (self.ops[&id].need, self.ops[&id].resources);
            // The workers the request may use at all: those placement may use whose caps
            // satisfy its platform.
            let usable: Vec<usize> = if placer.is_some() {
                (0..names.len())
                    .filter(|&i| self.matches(need, &names[i]))
                    .collect()
            } else {
                Vec::new()
            };
            let key = self.ops[&id].key.clone();
            let ask = Ask {
                operation: id,
                key: &key,
                resources: Resources::new(res[0], res[1]).with_gpus(res[2]),
                pool: need,
                candidates: &usable,
            };
            if expected.len() < ROUND {
                if let Some(placer) = placer.as_mut() {
                    self.stats.overbooked_skips += u64::from(placer.kept_off_overcharged(&ask));
                    if let Some(i) = placer.place(&ask) {
                        expected.push((id, names[i].clone()));
                        verdicts.push((id, Verdict::Servable));
                        continue;
                    }
                } else {
                    // The least loaded worker with room; of equals, the first by name.
                    let mut placed: Option<&WorkerId> = None;
                    for name in &names {
                        let cap = self.workers[name].capacity;
                        if fits_beside(cap, booked[name], res) && self.matches(need, name) {
                            let better = placed.is_none_or(|best| {
                                let load = |n: &WorkerId| (self.workers[n].capacity, booked[n]);
                                by_load(load(name), load(best)) == Ordering::Less
                            });
                            if better {
                                placed = Some(name);
                            }
                        }
                        if fits_whole(cap, res)
                            && !fits_beside(cap, booked[name], res)
                            && [0, 2].iter().any(|&i| booked[name].booked[i] > cap[i])
                            && self.matches(need, name)
                        {
                            self.stats.overbooked_skips += 1;
                        }
                    }
                    if let Some(name) = placed.cloned() {
                        let cap = self.workers[&name].capacity;
                        let b = booked.get_mut(&name).expect("live");
                        b.booked = add(b.booked, res);
                        b.leases += 1;
                        b.whole |= exclusive(cap, res);
                        expected.push((id, name));
                        verdicts.push((id, Verdict::Servable));
                        continue;
                    }
                }
            } else if !left_over {
                left_over = match placer.as_ref() {
                    Some(placer) => placer.fits(&ask),
                    None => names.iter().any(|n| {
                        fits_beside(self.workers[n].capacity, booked[n], self.ops[&id].resources)
                    }),
                };
            }
            let v = self.verdict(id);
            if expected.len() == ROUND && v != Verdict::Servable {
                self.stats.verdicts_past_cut += 1;
            }
            verdicts.push((id, v));
        }
        if expected.len() == ROUND {
            self.stats.full_rounds += 1;
            self.stats.cut_rounds += u64::from(left_over);
        }

        // Sort the effects by kind.
        let mut grants = Vec::new();
        let mut refusals = Vec::new();
        let mut waiting: BTreeMap<OperationId, Option<String>> = BTreeMap::new();
        for effect in effects {
            match effect {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g.clone()),
                Effect::Commit(ControlRecord::Refusal(r)) => refusals.push(r.clone()),
                Effect::Waiting(w) => {
                    let dup = waiting.insert(w.operation, w.reason.clone()).is_some();
                    self.ensure(!dup, "contract", || {
                        format!("two Waiting effects for {} in one round", w.operation)
                    });
                }
                other => self.fail("contract", &format!("a round emitted {other:?}")),
            }
        }

        // Waiting reasons, the unservable wait and refusals (I10, L2).
        let mut expected_refusals = Vec::new();
        for (id, v) in verdicts {
            let told_before = self.ops[&id].told.clone();
            let said = waiting.remove(&id);
            let now = self.now;
            let told = match (v, &told_before, said) {
                (Verdict::Servable, Some(_), Some(None)) | (Verdict::Servable, None, None) => None,
                (Verdict::Unservable | Verdict::Cordoned, _, Some(Some(reason))) => Some(reason),
                (Verdict::Unservable | Verdict::Cordoned, Some(before), None) => {
                    Some(before.clone())
                }
                (v, told, said) => {
                    let what = format!(
                        "the reference verdict on {id} is {v:?}, its callers were told \
                         {told:?}, the scheduler said {said:?}"
                    );
                    self.fail("I10", &what);
                }
            };
            let wait = self.wait;
            let op = self.ops.get_mut(&id).expect("queued");
            op.since = match v {
                Verdict::Unservable => Some(op.since.unwrap_or(now)),
                _ => None,
            };
            op.told = told;
            if let Some(since) = op.since
                && now >= since + wait
            {
                let reason = format!(
                    "{} (waited {} s for a worker that can run it)",
                    op.told.as_deref().unwrap_or_default(),
                    wait / 1_000
                );
                expected_refusals.push((id, reason));
            }
        }
        if let Some((id, said)) = waiting.into_iter().next() {
            self.fail(
                "I10",
                &format!("{id} is not queued, scheduler said {said:?}"),
            );
        }
        let got: Vec<(OperationId, String)> = refusals
            .iter()
            .map(|r| (r.operation, r.reason.clone()))
            .collect();
        if got != expected_refusals {
            let what =
                format!("refusals {got:?}, the reference verdict wants {expected_refusals:?}");
            self.fail("I10/L2", &what);
        }
        for (id, _) in &expected_refusals {
            let op = self.ops.get_mut(id).expect("queued");
            op.refusing = true;
            let key = queue_key(&op.qos, *id);
            self.queue.remove(&key);
            self.touched_ops.insert(*id);
        }

        // Grants: I1, I6, I7, I8 for each, then I11 for the round.
        for g in &grants {
            self.ensure(self.last_lease < Some(g.lease), "I1", || {
                format!("{} granted after {:?}", g.lease, self.last_lease)
            });
            self.last_lease = Some(g.lease);
            let Some(op) = self.ops.get(&g.operation) else {
                self.fail(
                    "I3",
                    &format!("{} granted for an unknown {}", g.lease, g.operation),
                );
            };
            self.ensure(Self::queued(op), "I3", || {
                format!(
                    "{} granted for {}, which is {:?}",
                    g.lease, g.operation, op.state
                )
            });
            let Some(w) = self.workers.get(&g.worker) else {
                self.fail(
                    "I7",
                    &format!("{} granted to unregistered {}", g.lease, g.worker),
                );
            };
            let room = Room {
                booked: w.booked,
                leases: w.leases,
                whole: w.whole,
            };
            let (cap, booked, res, need) = (w.capacity, w.booked, op.resources, op.need);
            if self.load {
                // The documented limits of the load rule: room but for the load, and the
                // cores charged to the worker, this lease's with them, within
                // `C (1 + s_cpu)`.
                let host = self.host(&g.worker);
                let request = Resources::new(res[0], res[1]).with_gpus(res[2]);
                self.ensure(host.admits(&request), "I6", || {
                    format!(
                        "{} of {} ({res:?}) to {} with {} millicores charged of {} ({booked:?} \
                         booked of {cap:?})",
                        g.lease,
                        g.operation,
                        g.worker,
                        host.left(),
                        host.cpu_limit()
                    )
                });
                self.stats.limit_fills +=
                    u64::from(host.left() + host.new_charge(&request) == host.cpu_limit());
                // Its record carries what it was charged: its cores cut to the worker's, and
                // the memory limit it runs under.
                self.ensure(
                    g.cost.cpu_millis == host.charge_of(&request)
                        && g.cost.limit_bytes == request.memory_bytes,
                    "I6",
                    || format!("{} of {} was charged {:?}", g.lease, g.operation, g.cost),
                );
            } else {
                self.ensure(fits_beside(cap, room, res), "I6", || {
                    format!(
                        "{} of {} ({res:?}) to {} with {booked:?} booked of {cap:?}",
                        g.lease, g.operation, g.worker
                    )
                });
            }
            self.ensure(self.live(&g.worker), "I7", || {
                format!("{} to {}, which is not live", g.lease, g.worker)
            });
            let ok = self.matches(need, &g.worker);
            self.ensure(ok, "I7", || {
                format!(
                    "{} to {}, whose caps do not satisfy the platform",
                    g.lease, g.worker
                )
            });
            self.ensure(!self.cordoned.contains(&g.worker), "I8", || {
                format!("{} to cordoned {}", g.lease, g.worker)
            });
            let exact = (0..3).any(|i| res[i] > 0 && booked[i] + res[i] == cap[i]);
            self.stats.exact_fills += u64::from(exact);
            self.grant(g.operation, g.lease, &g.worker);
        }
        let got: Vec<(OperationId, WorkerId)> = grants
            .iter()
            .map(|g| (g.operation, g.worker.clone()))
            .collect();
        if got != expected {
            let first = got
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(got.len().min(expected.len()));
            let what = format!(
                "the round granted {} leases, the reference placement {}; first difference \
                 at #{first}: got {:?}, want {:?}",
                got.len(),
                expected.len(),
                got.get(first),
                expected.get(first)
            );
            self.fail("I11", &what);
        }
        self.last_round = got;
        if let Some(placer) = placer {
            self.holds = placer.holds().clone();
        }
    }

    fn grant(&mut self, id: OperationId, lease: LeaseId, worker: &WorkerId) {
        let now = self.now;
        let op = self.ops.get_mut(&id).expect("exists");
        self.queue.remove(&queue_key(&op.qos, id));
        op.state = State::Leased {
            lease,
            worker: worker.clone(),
            committed: false,
        };
        op.told = None;
        op.since = None;
        let w = self.workers.get_mut(worker).expect("registered");
        w.whole |= exclusive(w.capacity, op.resources);
        w.charged
            .insert(lease, (op.key.clone(), op.resources[0].min(w.capacity[0])));
        w.booked = add(w.booked, op.resources);
        w.leases += 1;
        self.held.insert(
            lease,
            Held {
                op: id,
                start: None,
            },
        );
        self.granted_at.entry(id).or_insert(now);
        self.stats.grants += 1;
        self.touched_ops.insert(id);
        self.touched_workers.insert(worker.clone());
    }
}
