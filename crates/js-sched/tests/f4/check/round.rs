//! One placement round, checked against the reference placement (the least loaded worker
//! with room) and verdict.

use super::*;
use crate::load_reference::{Ask, Host, Placer};

impl Checker {
    /// Worker `name` as the load reference meets it: its capacity, the GPUs and VM slots it
    /// books, whether one lease holds it alone, and what each lease was charged.
    fn host(&self, name: &WorkerId) -> Host {
        let w = &self.workers[name];
        let charged = w.charged.values().cloned().collect();
        Host::new(
            name.clone(),
            w.capacity,
            w.booked.gpus,
            w.booked.vms,
            w.whole,
            charged,
        )
    }
}

impl Checker {
    /// One placement round, checked against the reference (I1, I6 to I8, I10, I11, L2).
    pub(super) fn round(&mut self, effects: &[Effect]) {
        let now = self.now;
        let (mut live, mut cordoned) = (Vec::new(), Vec::new());
        for (name, w) in &self.workers {
            if w.alive(now) {
                if self.cordons.contains_key(name) {
                    cordoned.push(name);
                } else {
                    live.push(name);
                }
            }
        }
        let caps = |names: &[&WorkerId]| -> Vec<(&NodeCaps, Resources)> {
            names
                .iter()
                .map(|n| (&self.workers[*n].caps, self.workers[*n].capacity))
                .collect()
        };
        let (live_caps, cordoned_caps) = (caps(&live), caps(&cordoned));
        let mut matching: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut verdicts: BTreeMap<(usize, Resources), Verdict> = BTreeMap::new();
        let mut no_room: BTreeSet<(usize, Resources)> = BTreeSet::new();
        // What this round has placed on each worker so far: its bookings, how many leases,
        // and whether one holds the node alone.
        let mut extra: BTreeMap<usize, (Resources, usize, bool)> = BTreeMap::new();
        let mut grants: Vec<(OperationId, WorkerId)> = Vec::new();
        let mut notes: Vec<(OperationId, Verdict)> = Vec::new();
        let mut room_short = false;
        let mut placer = self.load.then(|| {
            let hosts = live.iter().map(|n| self.host(n)).collect();
            Placer::new(hosts, TERM, self.next_seq, self.holds.clone())
        });
        for &(_, id) in &self.queue {
            let op = &self.ops[usize::try_from(id.0).expect("ids fit")];
            let class = (op.needs, op.request.resources);
            let needs = &self.needs[op.needs];
            let mut placed = false;
            // An operation with farm runs avoids their workers while another could run
            // it, so its fit is its own, not its class's.
            let avoids = !op.avoid.is_empty();
            let m = matching.entry(op.needs).or_insert_with(|| {
                (0..live.len())
                    .filter(|&i| needs.matches(&self.workers[live[i]].caps))
                    .collect()
            });
            let avoided = |i: usize| op.avoid.contains(live[i]);
            let elsewhere = m
                .iter()
                .copied()
                .any(|i| !avoided(i) && fits(self.workers[live[i]].capacity, op.request.resources));
            // What worker `i` holds now: its bookings, how many leases, and whether one
            // holds it alone.
            let held_on = |extra: &BTreeMap<usize, (Resources, usize, bool)>, i: usize| {
                let w = &self.workers[live[i]];
                let (more, leases, whole) = extra.get(&i).copied().unwrap_or_default();
                (
                    add(w.booked, more),
                    w.leases.len() + leases,
                    w.whole || whole,
                )
            };
            if grants.len() < PLACEMENT_ROUND && (avoids || !no_room.contains(&class)) {
                let fit = if let Some(placer) = placer.as_mut() {
                    // The load rule: the reference placement of `load_reference`.
                    let usable: Vec<usize> = m
                        .iter()
                        .copied()
                        .filter(|&i| !(elsewhere && avoided(i)))
                        .collect();
                    let ask = Ask {
                        operation: id,
                        key: &op.request.key,
                        resources: op.request.resources,
                        pool: op.needs,
                        candidates: &usable,
                    };
                    placer.place(&ask)
                } else {
                    // The least loaded worker with room; of equals, the first by name.
                    m.iter()
                        .copied()
                        .filter(|&i| {
                            let (booked, leases, whole) = held_on(&extra, i);
                            room(
                                self.workers[live[i]].capacity,
                                booked,
                                whole,
                                leases,
                                op.request.resources,
                            ) && !(elsewhere && avoided(i))
                        })
                        .min_by(|&a, &b| {
                            let load = |i: usize| {
                                let (booked, leases, _) = held_on(&extra, i);
                                (self.workers[live[i]].capacity, booked, leases)
                            };
                            by_load(load(a), load(b))
                        })
                };
                match fit {
                    Some(i) => {
                        let alone = exclusive(self.workers[live[i]].capacity, op.request.resources);
                        let e = extra.entry(i).or_default();
                        *e = (add(e.0, op.request.resources), e.1 + 1, e.2 || alone);
                        grants.push((id, live[i].clone()));
                        placed = true;
                    }
                    None => {
                        if !avoids {
                            no_room.insert(class);
                        }
                    }
                }
            }
            let verdict = if placed {
                Verdict::Servable
            } else {
                *verdicts.entry(class).or_insert_with(|| {
                    verdict(&live_caps, &cordoned_caps, needs, op.request.resources)
                })
            };
            room_short |= !placed && verdict == Verdict::Servable;
            if op.wait.is_some() || verdict != Verdict::Servable {
                notes.push((id, verdict));
            }
        }
        if room_short {
            self.hit("servable work waits for room");
        }
        if grants.len() == PLACEMENT_ROUND {
            self.hit("full round (PLACEMENT_ROUND grants)");
        }

        // Split what the round emitted.
        let mut told: BTreeMap<OperationId, Option<String>> = BTreeMap::new();
        let mut refused: BTreeMap<OperationId, String> = BTreeMap::new();
        let mut granted: Vec<&LeaseGrant> = Vec::new();
        for effect in effects {
            match effect {
                Effect::Waiting(w) => {
                    if told.insert(w.operation, w.reason.clone()).is_some() {
                        self.violated("I10", format!("{}: told twice in one round", w.operation));
                    }
                }
                Effect::Commit(ControlRecord::Refusal(r)) => {
                    refused.insert(r.operation, r.reason.clone());
                }
                Effect::Commit(ControlRecord::Lease(g)) => granted.push(g),
                other => self.violated("I2", format!("a placement round emitted {other:?}")),
            }
        }

        // Grants: the explicit invariants first, then the reference placement.
        let mut booked_now: BTreeMap<WorkerId, (Resources, usize, bool)> = BTreeMap::new();
        // Under the load rule, each granted worker as the grants of this round leave it.
        let mut hosts_now: BTreeMap<WorkerId, Host> = BTreeMap::new();
        for g in &granted {
            if self.next_seq != g.lease.seq || g.lease.term != TERM {
                self.violated(
                    "I1",
                    format!("{} granted; next lease is seq {}", g.lease, self.next_seq),
                );
            }
            self.next_seq += 1;
            let Some(w) = self.workers.get(&g.worker) else {
                self.violated("I7", format!("{g:?} to an unregistered worker"));
            };
            let op = self.op(g.operation);
            if self.load {
                // The documented limits of the load rule: room but for the load, and the
                // cores charged to the worker, this lease's with them, within
                // `C (1 + s_cpu)`. The lease's record carries what it was charged: its
                // cores cut to the worker's, and the memory limit it runs under.
                let host = hosts_now
                    .entry(g.worker.clone())
                    .or_insert_with(|| self.host(&g.worker));
                let request = op.request.resources;
                if !host.admits(&request) {
                    self.violated(
                        "I6",
                        format!(
                            "{g:?} placed with {} millicores charged of {} on capacity {:?}",
                            host.left(),
                            host.cpu_limit(),
                            w.capacity
                        ),
                    );
                }
                if g.cost.cpu_millis != host.charge_of(&request)
                    || g.cost.limit_bytes != request.memory_bytes
                {
                    self.violated("I6", format!("{g:?} was charged {:?}", g.cost));
                }
                host.book(&op.request.key, &request);
            } else {
                let held = booked_now.entry(g.worker.clone()).or_insert((
                    w.booked,
                    w.leases.len(),
                    w.whole,
                ));
                if !room(w.capacity, held.0, held.2, held.1, op.request.resources) {
                    self.violated(
                        "I6",
                        format!(
                            "{g:?} placed beside {:?} booked ({} leases, alone: {}) on capacity {:?}",
                            held.0, held.1, held.2, w.capacity
                        ),
                    );
                }
                *held = (
                    add(held.0, op.request.resources),
                    held.1 + 1,
                    held.2 || exclusive(w.capacity, op.request.resources),
                );
            }
            if !w.alive(now) || !self.needs[op.needs].matches(&w.caps) {
                self.violated(
                    "I7",
                    format!("{g:?}: worker silent or its caps do not satisfy the platform"),
                );
            }
            if self.cordons.contains_key(&g.worker) {
                self.violated("I8", format!("{g:?} to a cordoned worker"));
            }
        }
        let got: Vec<(OperationId, WorkerId)> = granted
            .iter()
            .map(|g| (g.operation, g.worker.clone()))
            .collect();
        if got != grants {
            self.violated(
                "I11",
                format!("placement differs from the reference\n  expected {grants:?}\n  got      {got:?}"),
            );
        }

        if let Some(placer) = placer {
            self.holds = placer.holds().clone();
        }

        // Verdicts: who is told what, and who is refused.
        let wait_ms = self.wait_ms;
        for (id, v) in notes {
            let reason = told.remove(&id);
            let wait = self.op(id).wait;
            if v == Verdict::Servable {
                if reason != Some(None) {
                    self.violated(
                        "I10",
                        format!("{id} is servable again but its callers got {reason:?}"),
                    );
                }
                let op = self.op_mut(id);
                op.wait = None;
                op.told = None;
                self.hit("servable again");
                continue;
            }
            let since = match wait {
                Some(w) if w.verdict.refusable() && v.refusable() => w.since,
                _ => now,
            };
            let changed = wait.is_none_or(|w| w.verdict != v);
            if wait.is_some_and(|w| w.verdict == Verdict::Cordoned) && v.refusable() {
                self.hit("a wait for a cordon became refusable");
            }
            let text = match reason {
                Some(Some(text)) if v.states(&text) => Some(text),
                None if !changed => None,
                other => {
                    self.violated("I10", format!("{id} waits ({v:?}); callers told {other:?}"))
                }
            };
            if v == Verdict::Cordoned {
                self.hit("waits for a cordon");
            }
            let op = self.op_mut(id);
            op.wait = Some(Wait { since, verdict: v });
            if let Some(text) = text {
                op.told = Some(text);
            }
            if v.refusable() && now >= since + wait_ms {
                let reason = format!(
                    "{} (waited {} s for a worker that can run it)",
                    op.told.as_deref().unwrap_or_default(),
                    wait_ms / 1_000
                );
                let qos = op.request.qos.clone();
                self.queue.remove(&(Reverse(qos), id));
                if refused.remove(&id).as_ref() != Some(&reason) {
                    self.violated(
                        "I10/L2",
                        format!("{id} due for refusal with {reason:?}; not refused so"),
                    );
                }
            }
        }
        if let Some((id, reason)) = told.into_iter().next() {
            self.violated(
                "I10",
                format!("{id} told {reason:?} with no verdict to tell"),
            );
        }
        if let Some((id, reason)) = refused.into_iter().next() {
            self.violated(
                "I10/L2",
                format!("{id} refused early or without a stated reason: {reason:?}"),
            );
        }

        for (g, (id, _)) in granted.iter().zip(&grants) {
            let resources = self.op(*id).request.resources;
            let key = self.op(*id).request.key.clone();
            let w = self.workers.get_mut(&g.worker).expect("checked");
            w.booked = add(w.booked, resources);
            w.whole |= exclusive(w.capacity, resources);
            w.leases.insert(g.lease);
            let cores = resources.cpu_millis.min(w.capacity.cpu_millis);
            w.charged.insert(g.lease, (key, cores));
            self.touched_workers.insert(g.worker.clone());
            let qos = self.op(*id).request.qos.clone();
            self.queue.remove(&(Reverse(qos), *id));
            self.op_mut(*id).holding = Some(g.lease);
            self.leases.insert(
                g.lease,
                Lease {
                    op: *id,
                    worker: g.worker.clone(),
                    committed: false,
                    running: false,
                    sent: None,
                },
            );
            self.hit("granted");
        }
    }
}
