//! Which live workers can run a request, for one placement round.
//!
//! A cordoned worker is not one of them: placement skips it, and work only it could
//! run waits with that reason for as long as the cordon lasts; it is never refused for
//! it (a cordon is temporary by intent). A worker whose phase is not `serving`
//! ([`NodePhase`]: switching its GUI session, or quarantined) is held back the
//! same way, its phase named beside it. A worker can run a request if it reports a
//! driver that serves the request's lease kind ([`LeaseKind::drivers`]), its
//! capabilities satisfy the request's platform (`js_caps::Request::matches`) and its
//! whole capacity holds the request vector. It can run it now if, for an `action`, its
//! free room holds the vector and no whole-machine lease holds it; for a
//! `whole_machine` lease, if it holds no lease at all. Matches are memoised per
//! distinct platform request and kind within the round, so a queue of many actions
//! with the same platform matches each live worker once.

use std::collections::{BTreeMap, BTreeSet};

use js_caps::NodeCaps;
use js_types::sched::{FarmTime, LeaseKind, NodePhase, Resources, WorkerId};

use js_types::sched::LeaseId;
use js_types::sched::usage::LeaseCost;

use crate::admission::{
    Admission, Blocked, Left, PlacementLoad, Ratio, Refusal, binding, clamped, pick,
};
use crate::cordon::Cordons;
use crate::input::Request;
use crate::load::Load;
use crate::scheduler::Worker;

/// Whether some live worker could run a request, ignoring what is booked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// A live worker satisfies the platform and is large enough.
    Servable,
    /// None is; why, for the request's callers.
    Unservable(String),
    /// None that placement may use is, but a cordoned one is; why, naming them. The
    /// work waits: a cordon is temporary by intent, so it is not refused for it.
    Cordoned(String),
}

/// One request to place: what it is, which workers are out, and what it will be charged.
pub(crate) struct Attempt<'a> {
    pub(crate) request: &'a Request,
    /// Workers held for a whole-machine lease or a blocked head ahead of it.
    pub(crate) reserved: &'a BTreeSet<WorkerId>,
    /// Workers its earlier runs ended on as the farm's fault.
    pub(crate) avoid: &'a BTreeSet<WorkerId>,
    /// A key floor raised its memory: a worker it would own whole is out.
    pub(crate) not_whole: bool,
    /// What it is charged if placed now (before the clamp to a worker's capacity).
    pub(crate) cost: LeaseCost,
    /// The lease it gets if placed now.
    pub(crate) lease: LeaseId,
}

/// Where a request was placed.
pub(crate) struct Placed {
    pub(crate) worker: WorkerId,
    /// What was booked on the worker.
    pub(crate) booked: Resources,
    /// The iOS device booked, by id.
    pub(crate) device: Option<String>,
    /// What the lease is charged.
    pub(crate) cost: LeaseCost,
}

/// The outcome of [`Servable::fit`].
pub(crate) struct Fit {
    pub(crate) placed: Option<Placed>,
    /// Why the load kept it from a worker, when some worker had room but for that.
    pub(crate) blocked: Option<Blocked>,
}

/// The live workers of one round, and the platform matches made so far.
#[derive(Debug)]
pub(crate) struct Servable {
    /// The farm time of the round.
    now: FarmTime,
    /// Live workers placement may use, in name order.
    live: Vec<WorkerId>,
    /// Live workers placement skips because they are cordoned or not serving, in name
    /// order.
    cordoned: Vec<WorkerId>,
    /// For each platform request and lease kind seen this round, the indices into
    /// `live` of the workers that serve the kind and satisfy the request.
    memo: Vec<((js_caps::Request, LeaseKind), Vec<usize>)>,
}

/// Whether a node that offers `caps` runs a driver serving `kind`.
pub(crate) fn serves(caps: &NodeCaps, kind: LeaseKind) -> bool {
    caps.drivers.iter().any(|driver| kind.served_by(driver))
}

impl Servable {
    /// The round's view of `workers` at `now`, without the `cordons` and the workers
    /// that are not serving.
    pub(crate) fn new(
        workers: &BTreeMap<WorkerId, Worker>,
        cordons: &Cordons,
        now: FarmTime,
    ) -> Self {
        let (cordoned, live) = workers
            .iter()
            .filter(|(_, w)| w.alive(now))
            .map(|(name, _)| name.clone())
            .partition(|name| cordons.skips(name) || workers[name].phase != NodePhase::Serving);
        Self {
            now,
            live,
            cordoned,
            memo: Vec::new(),
        }
    }

    /// Where in `memo` the live workers that serve `request`'s kind and satisfy its
    /// platform are listed.
    fn matching(&mut self, workers: &BTreeMap<WorkerId, Worker>, request: &Request) -> usize {
        let key = (request.needs.clone(), request.kind);
        if let Some(at) = self.memo.iter().position(|(seen, _)| *seen == key) {
            return at;
        }
        let matching = self
            .live
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                let caps = &workers[*name].caps;
                serves(caps, request.kind) && request.needs.matches(caps)
            })
            .map(|(i, _)| i)
            .collect();
        self.memo.push((key, matching));
        self.memo.len() - 1
    }

    /// The least loaded live worker not in `reserved` that serves `request`'s kind,
    /// satisfies its platform and has room for it now. Books it there and returns the
    /// worker with what was booked: the request vector for an action, the worker's whole
    /// capacity for a whole-machine lease; the iOS device, for a request that asks for
    /// one ([`Worker::free_device`]); and what the lease is charged ([`LeaseCost`]).
    ///
    /// The platform and room are decided first and the load only ranks the workers that
    /// pass: a Mac is never the least loaded worker for Linux work. A worker in
    /// `reserved` (held for a whole-machine lease or for a blocked head), one that a
    /// whole-machine lease holds, and one that is draining or cordoned
    /// ([`Servable::new`]) are out before the load predicate is read and before any
    /// ranking, so a worker that is being emptied for someone is never the least loaded
    /// one for small work.
    ///
    /// For an action under [`PlacementLoad::On`], room is the load predicate
    /// ([`Admission`]) and the ranking is by what the worker is charged
    /// ([`Worker::load_charged`]); under `Shadow` the booked rule decides and the
    /// predicate is only counted on the worker chosen; otherwise the booked rule and the
    /// booked ranking decide ([`Worker::has_room`], [`Worker::load`]). A whole-machine
    /// lease, and an action at the whole host's memory, go only to a worker with no lease,
    /// so every candidate has nothing booked and the first by name is taken. Such an
    /// action waits until some worker's free memory meets its request; nothing reserves a
    /// worker for it.
    ///
    /// With `not_whole` (a request a key floor raised) a worker whose memory the request
    /// equals is skipped: there it would own the host. [`Servable::roomy`] tells whether
    /// any worker is left.
    ///
    /// A worker in `avoid` (one the operation's earlier runs ended on as the farm's
    /// fault) is skipped while a worker placement may use outside it could run the
    /// request once its bookings end: the request waits for that one. Only when none
    /// could is a worker in `avoid` taken, so a farm of one node still runs it.
    pub(crate) fn fit(
        &mut self,
        workers: &mut BTreeMap<WorkerId, Worker>,
        adm: &mut Admission,
        attempt: &Attempt<'_>,
    ) -> Fit {
        let request = attempt.request;
        let at = self.matching(workers, request);
        let skip = self.avoided(at, workers, request, attempt.avoid);
        let by_load = adm.by_load() && request.kind == LeaseKind::Action;
        let now = self.now;
        // The workers that pass everything but the load predicate.
        let candidates: Vec<&WorkerId> = self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .filter(|name| {
                !attempt.reserved.contains(*name)
                    && !skip.contains(*name)
                    && workers[*name].has_room(request, by_load)
                    && (!attempt.not_whole || !workers[*name].exclusive(request))
            })
            .collect();
        let mut blocked = None;
        let chosen = if by_load {
            let (admitted, why) = Self::admitted(adm, workers, &candidates, attempt, now);
            blocked = why;
            admitted
                .into_iter()
                .min_by_key(|(_, load)| *load)
                .map(|(name, _)| name)
        } else {
            // The first of the least loaded: `live` is in name order.
            candidates
                .iter()
                .copied()
                .min_by_key(|name| workers[*name].load())
        };
        let Some(name) = chosen.cloned() else {
            if let Some(why) = blocked {
                adm.count_blocked(why);
            }
            return Fit {
                placed: None,
                blocked,
            };
        };
        let worker = workers.get_mut(&name).expect("live workers are registered");
        let (cost, left) = Self::charged(adm, worker, attempt, now);
        if request.kind == LeaseKind::Action {
            if by_load {
                adm.count_admitted(&request.key, &cost, left.fallback);
            } else if adm.cfg.mode == PlacementLoad::Shadow {
                let ok = worker
                    .load_state
                    .admits(&cost, &left, &worker.capacity, now, &adm.cfg);
                adm.count_shadow(ok.is_ok());
            }
            if cost.cold {
                worker.load_state.note_cold(now, &adm.cfg);
            }
        }
        let booked = match request.kind {
            LeaseKind::WholeMachine => {
                worker.whole = true;
                worker.capacity
            }
            LeaseKind::Action | LeaseKind::Vm => {
                worker.whole = worker.exclusive(request);
                request.resources
            }
        };
        worker.booked = worker.booked.saturating_add(booked);
        worker.leases += 1;
        match request.kind {
            LeaseKind::Action => worker.load_state.insert(attempt.lease, cost, false),
            LeaseKind::Vm => worker.load_state.insert(attempt.lease, cost, true),
            LeaseKind::WholeMachine => {}
        }
        let device = worker.free_device(&request.needs).map(str::to_owned);
        worker.devices.extend(device.clone());
        Fit {
            placed: Some(Placed {
                worker: name,
                booked,
                device,
                cost,
            }),
            blocked: None,
        }
    }

    /// The candidates the predicate admits, each with its load and charge, and why none
    /// was, if none: `Load` if any was refused on CPU or memory, `ColdCap` if the cold
    /// cap alone refused. `None` when there was no candidate to refuse.
    fn admitted<'a>(
        adm: &Admission,
        workers: &BTreeMap<WorkerId, Worker>,
        candidates: &[&'a WorkerId],
        attempt: &Attempt<'_>,
        now: FarmTime,
    ) -> (Vec<(&'a WorkerId, Load)>, Option<Blocked>) {
        let mut admitted = Vec::new();
        let (mut load, mut cold) = (false, false);
        for &name in candidates {
            let w = &workers[name];
            let cost = clamped(attempt.cost, &w.capacity);
            let left = w.load_state.left(now, &w.capacity, &adm.cfg);
            match w
                .load_state
                .admits(&cost, &left, &w.capacity, now, &adm.cfg)
            {
                Ok(()) => admitted.push((name, w.load_charged(&left))),
                Err(Refusal::ColdCap) => cold = true,
                Err(_) => load = true,
            }
        }
        let why = if !admitted.is_empty() {
            None
        } else if load {
            Some(Blocked::Load)
        } else if cold {
            Some(Blocked::ColdCap)
        } else {
            None
        };
        (admitted, why)
    }

    /// The cost `attempt` is charged on `worker`, and what the worker is charged before it.
    fn charged(
        adm: &Admission,
        worker: &Worker,
        attempt: &Attempt<'_>,
        now: FarmTime,
    ) -> (LeaseCost, Left) {
        let cost = match attempt.request.kind {
            LeaseKind::Action => clamped(attempt.cost, &worker.capacity),
            // A VM is charged what it books; a whole-machine lease is not on the ledger.
            LeaseKind::Vm | LeaseKind::WholeMachine => attempt.cost,
        };
        let left = worker.load_state.left(now, &worker.capacity, &adm.cfg);
        (cost, left)
    }

    /// The worker a blocked head of the queue holds: of the workers `request` could run
    /// on but for the load, not in `reserved` or skipped for `avoid` and not owned whole
    /// by the request, the one with the smallest binding ratio, keeping `kept` while its
    /// ratio is within a tenth of the best ([`pick`]). `None` when no worker could take
    /// it at all.
    pub(crate) fn hold_for_head(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        adm: &Admission,
        attempt: &Attempt<'_>,
        kept: Option<&WorkerId>,
    ) -> Option<WorkerId> {
        let request = attempt.request;
        let at = self.matching(workers, request);
        let skip = self.avoided(at, workers, request, attempt.avoid);
        let now = self.now;
        let ratios: Vec<(&WorkerId, Ratio)> = self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .filter(|name| {
                let w = &workers[*name];
                !attempt.reserved.contains(*name)
                    && !skip.contains(*name)
                    && !w.exclusive(request)
                    && w.has_room(request, true)
            })
            .map(|name| {
                let w = &workers[name];
                let cost = clamped(attempt.cost, &w.capacity);
                let left = w.load_state.left(now, &w.capacity, &adm.cfg);
                let ratio = binding(
                    (
                        left.cpu.saturating_add(cost.cpu_millis),
                        w.capacity.cpu_millis,
                    ),
                    (
                        left.mem.saturating_add(cost.memory_bytes),
                        w.capacity.memory_bytes,
                    ),
                );
                (name, ratio)
            })
            .collect();
        pick(&ratios, kept).cloned()
    }

    /// The pool `request` is run from: the platform and kind it needs, shared by every
    /// request that needs the same.
    pub(crate) fn pool(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> usize {
        self.matching(workers, request)
    }

    /// Whether some live worker placement may use that serves `request` could take it
    /// without being owned whole: one with more memory than the request asks for.
    pub(crate) fn roomy(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> bool {
        let at = self.matching(workers, request);
        self.memo[at].1.iter().any(|&i| {
            let w = &workers[&self.live[i]];
            w.capacity.fits(&request.resources)
                && w.capacity.memory_bytes > request.resources.memory_bytes
        })
    }

    /// The worker to hold for a whole-machine `request` that fits nowhere now: `kept`
    /// if it still could run it, else the one that could which holds the fewest leases,
    /// then has the least booked, then comes first by name. Never one in `reserved`,
    /// nor one in `avoid` while [`Servable::fit`] skips it. Only a whole-machine lease
    /// holds a worker (the caller asks only for one); an action at a worker's whole
    /// memory waits until some worker has no lease, and nothing reserves one for it.
    pub(crate) fn reserve(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
        reserved: &BTreeSet<WorkerId>,
        avoid: &BTreeSet<WorkerId>,
        kept: Option<&WorkerId>,
    ) -> Option<WorkerId> {
        let at = self.matching(workers, request);
        let skip = self.avoided(at, workers, request, avoid);
        let candidates: Vec<&WorkerId> = self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .filter(|name| {
                !reserved.contains(*name)
                    && !skip.contains(*name)
                    && workers[*name].capacity.fits(&request.resources)
            })
            .collect();
        if let Some(kept) = kept.filter(|kept| candidates.contains(kept)) {
            return Some(kept.clone());
        }
        candidates
            .into_iter()
            .min_by_key(|name| {
                let w = &workers[*name];
                (w.leases, w.booked)
            })
            .cloned()
    }

    /// The workers of `avoid` that placement skips for `request`, whose matching live
    /// workers are `memo[at]`: all of them while a matching worker outside `avoid`
    /// could run the request once its bookings end, else none.
    fn avoided<'a>(
        &self,
        at: usize,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
        avoid: &'a BTreeSet<WorkerId>,
    ) -> &'a BTreeSet<WorkerId> {
        static NONE: BTreeSet<WorkerId> = BTreeSet::new();
        let elsewhere =
            self.memo[at].1.iter().map(|&i| &self.live[i]).any(|name| {
                !avoid.contains(name) && workers[name].capacity.fits(&request.resources)
            });
        if elsewhere { avoid } else { &NONE }
    }

    /// Whether any live worker could run `request` once its bookings end, and if none
    /// could, why. When only cordoned workers could, that is the verdict; the cordoned
    /// workers are looked at only then.
    pub(crate) fn verdict(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> Verdict {
        let verdict = self.uncordoned_verdict(workers, request);
        if verdict == Verdict::Servable {
            return verdict;
        }
        let could: Vec<(&WorkerId, &NodePhase)> = self
            .cordoned
            .iter()
            .map(|name| (name, &workers[name].phase))
            .filter(|(name, _)| {
                let w = &workers[*name];
                serves(&w.caps, request.kind)
                    && request.needs.matches(&w.caps)
                    && w.capacity.fits(&request.resources)
            })
            .collect();
        if could.is_empty() {
            return verdict;
        }
        let held = if could.iter().all(|(_, phase)| **phase == NodePhase::Serving) {
            "cordoned"
        } else {
            "cordoned or not serving"
        };
        let named: Vec<String> = could
            .iter()
            .map(|(name, phase)| match phase {
                NodePhase::Serving => name.as_str().to_owned(),
                phase => format!("{name} ({})", phase.name()),
            })
            .collect();
        Verdict::Cordoned(format!(
            "every live worker that can run it is {held}: {}",
            named.join(", ")
        ))
    }

    /// The cordoned or not serving live workers that could run `request` (serve its kind,
    /// satisfy its platform, fit its size): the ones [`Verdict::Cordoned`] names. Empty
    /// when none could.
    pub(crate) fn cordoned_could(
        workers: &BTreeMap<WorkerId, Worker>,
        cordons: &Cordons,
        now: FarmTime,
        request: &Request,
    ) -> Vec<WorkerId> {
        Self::new(workers, cordons, now)
            .cordoned
            .into_iter()
            .filter(|name| {
                let w = &workers[name];
                serves(&w.caps, request.kind)
                    && request.needs.matches(&w.caps)
                    && w.capacity.fits(&request.resources)
            })
            .collect()
    }

    /// [`Self::verdict`] over the workers placement may use.
    fn uncordoned_verdict(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> Verdict {
        if self.live.is_empty() {
            let why = if self.cordoned.is_empty() {
                "no worker is connected"
            } else if self
                .cordoned
                .iter()
                .all(|name| workers[name].phase == NodePhase::Serving)
            {
                "every connected worker is cordoned"
            } else {
                "every connected worker is cordoned or not serving"
            };
            return Verdict::Unservable(why.to_owned());
        }
        let kind = request.kind;
        let serving = self
            .live
            .iter()
            .filter(|name| serves(&workers[*name].caps, kind))
            .count();
        if serving == 0 {
            return Verdict::Unservable(format!(
                "none of the {} live worker(s) serves lease kind {kind}: that needs a \
                 driver among {}",
                self.live.len(),
                kind.drivers().join(", ")
            ));
        }
        let at = self.matching(workers, request);
        let matching = &self.memo[at].1;
        if matching.is_empty() {
            let workers_named = if serving == self.live.len() {
                format!("{serving} live worker(s)")
            } else {
                format!("{serving} live worker(s) serving lease kind {kind}")
            };
            let (closest, unmet) = self
                .live
                .iter()
                .filter(|name| serves(&workers[*name].caps, kind))
                .map(|name| (name, request.needs.unmet(&workers[name].caps)))
                .min_by_key(|(_, unmet)| unmet.len())
                .expect("there is a live worker");
            let unmet: Vec<String> = unmet.iter().map(ToString::to_string).collect();
            return Verdict::Unservable(format!(
                "none of the {workers_named} satisfies the action's platform; the closest, \
                 {closest}, lacks {}",
                unmet.join(", ")
            ));
        }
        let large_enough = matching
            .iter()
            .any(|&i| workers[&self.live[i]].capacity.fits(&request.resources));
        if large_enough {
            return Verdict::Servable;
        }
        let r = request.resources;
        Verdict::Unservable(format!(
            "the {} live worker(s) that satisfy the action's platform are all smaller than \
             its request ({} millicores, {} bytes of memory, {} GPU(s), {} VM slot(s))",
            matching.len(),
            r.cpu_millis,
            r.memory_bytes,
            r.gpus,
            r.vms
        ))
    }
}
