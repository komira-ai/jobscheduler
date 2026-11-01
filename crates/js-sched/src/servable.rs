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
use js_whole::cost::DrainCost;

use std::sync::Arc;

use js_types::sched::LeaseId;
use js_types::sched::usage::LeaseCost;

use crate::admission::{
    Admission, Blocked, Left, Profile, Ratio, Refusal, binding, clamped, new_charge, start_time,
};
use crate::cordon::Cordons;
use crate::input::Request;
use crate::load::{Charge, HostFacts, LeaseFacts, Load, SLOTS, SlotHost, SlotLoad, least_loaded};
use crate::scheduler::Worker;
use crate::unservable::Unservable;

/// Whether some live worker could run a request, ignoring what is booked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// A live worker satisfies the platform and is large enough.
    Servable,
    /// None is; why, for the request's callers.
    Unservable(Unservable),
    /// None that placement may use is, but a cordoned one is; why, naming them. The
    /// work waits: a cordon is temporary by intent, so it is not refused for it.
    Cordoned(Unservable),
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
    /// How it is charged over time, if it is (see [`Admission::profile_of`]).
    pub(crate) profile: Option<Arc<Profile>>,
    /// The lease it gets if placed now.
    pub(crate) lease: LeaseId,
    /// `L` of the lease's key where it is charged by a curve, else 1: the minutes the
    /// score's fit looks at.
    pub(crate) fit_minutes: u8,
}

/// A candidate the predicate admitted, with what it computed for it.
struct Passed<'a> {
    name: &'a WorkerId,
    /// The previous ranking's load of the worker.
    load: Load,
    /// What the worker is charged before the new lease.
    left: Left,
    /// What the new lease is charged on it (clamped to its capacity).
    cost: LeaseCost,
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

/// The new lease's charge `slot` minutes ahead on `worker` as the predicate counts it, as
/// `(millicores, bytes)`, where `left` is what the worker is charged now. It is the one
/// function behind `Scheduler::charge_at` and the figures the score reads, so the two
/// cannot differ. `cost` is the lease's cost before the clamp to the worker; `profile` is
/// how it is charged over time, which an action that would own the worker whole is not
/// (it is charged flat). An action that owns its worker ([`Request::owns_worker`]) is
/// charged here, as the predicate counts it, at its predicted cost clamped to the worker;
/// once placed, the ledger holds the worker's whole capacity for it
/// ([`Worker::charged_whole`]), which [`Scheduler::load_at`] shows. The predicate is not
/// given the capacity: a worker's own processes make it measured-busy, and the predicate
/// would refuse every worker.
///
/// [`Scheduler::load_at`]: crate::Scheduler::load_at
pub(crate) fn charge_on(
    worker: &Worker,
    request: &Request,
    cost: LeaseCost,
    profile: Option<&Arc<Profile>>,
    left: &Left,
    slot: usize,
) -> (u64, u64) {
    let cost = clamped(cost, &worker.capacity);
    let profile = profile.filter(|_| !worker.exclusive(request));
    new_charge(&cost, profile.map(|p| &**p), left, &worker.capacity, slot)
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
    /// For an action under [`crate::PlacementLoad::On`], room is the load predicate
    /// ([`Admission`]) and the ranking is by what the worker is charged
    /// ([`Worker::load_charged`]); under `Off` (the rollback switch) the booked rule and the
    /// booked ranking decide ([`Worker::has_room`], [`Worker::load`]). A whole-machine
    /// lease, an action that owns its worker and an action at the whole host's memory go only
    /// to a worker with no lease,
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
        // A VM is placed by its booking, and under load admission also held to the
        // flat-memory guard.
        let vm_by_load = adm.by_load() && request.kind == LeaseKind::Vm;
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
                    && (!vm_by_load || Self::vm_within_flat_memory(adm, &workers[*name], attempt))
            })
            .collect();
        let mut blocked = None;
        let chosen = if by_load {
            let (admitted, why) = Self::admitted(adm, workers, &candidates, attempt, now);
            blocked = why;
            Self::scored(adm, workers, &admitted, attempt, now)
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
        let profile = Self::profile_on(worker, attempt).cloned();
        if request.kind == LeaseKind::Action {
            if by_load {
                adm.count_admitted(&request.key, &cost, left.fallback, profile.as_deref());
            }
            if cost.cold {
                worker.load_state.note_cold(now, &adm.cfg);
            }
        }
        let booked = worker.booking(request);
        match request.kind {
            LeaseKind::WholeMachine => worker.whole = true,
            LeaseKind::Action | LeaseKind::Vm => worker.whole = worker.exclusive(request),
        }
        worker.booked = worker.booked.saturating_add(booked);
        worker.leases += 1;
        match request.kind {
            LeaseKind::Action => worker
                .load_state
                .insert(attempt.lease, cost, false, profile),
            LeaseKind::Vm => worker.load_state.insert(attempt.lease, cost, true, None),
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

    /// Whether a VM of `attempt` passes the flat-memory guard on `worker`: its booked
    /// memory is its flat cost, summed with the flat costs of the leases there and the
    /// other VMs' bookings.
    fn vm_within_flat_memory(adm: &Admission, worker: &Worker, attempt: &Attempt<'_>) -> bool {
        worker
            .load_state
            .flat_memory_refusal(&attempt.cost, &worker.capacity, &adm.cfg)
            .is_none()
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
    ) -> (Vec<Passed<'a>>, Option<Blocked>) {
        let mut admitted = Vec::new();
        let (mut load, mut cold) = (false, false);
        for &name in candidates {
            let w = &workers[name];
            let cost = clamped(attempt.cost, &w.capacity);
            let left = w.load_state.left(now, &w.capacity, &adm.cfg);
            match w.load_state.admits(
                &cost,
                Self::profile_on(w, attempt).map(|p| &**p),
                &left,
                &w.capacity,
                now,
                &adm.cfg,
            ) {
                Ok(()) => admitted.push(Passed {
                    name,
                    load: w.load_charged(&left),
                    left,
                    cost,
                }),
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

    /// How `attempt` is charged over time on `worker`: as its profile says, except that an
    /// action at the whole host's memory runs alone and is charged flat.
    fn profile_on<'a>(worker: &Worker, attempt: &'a Attempt<'_>) -> Option<&'a Arc<Profile>> {
        attempt
            .profile
            .as_ref()
            .filter(|_| !worker.exclusive(attempt.request))
    }

    fn loads<'a>(admitted: &[Passed<'a>]) -> Vec<(&'a WorkerId, Load)> {
        admitted.iter().map(|p| (p.name, p.load)).collect()
    }

    /// The worker among `admitted`, the workers that passed everything including the
    /// predicate, that the score chooses. A worker the request would own whole
    /// ([`Worker::exclusive`], decided per worker) keeps the previous rule and is not
    /// scored: the score ranks the workers that stay shared, so a mixed pool where the
    /// request fills a small worker but not a large one scores the large ones. When every
    /// admitted worker would be owned whole, the least loaded is taken. Nothing is scored
    /// for a single candidate. Only an action reaches here: its callers place by load
    /// for actions only.
    ///
    /// The score reads each worker's left side and the new lease's charge at every slot
    /// (minutes `0..=HORIZON` and `H+`), the figures [`Scheduler::load_at`] and
    /// [`Scheduler::charge_at`] give; at slot 0 they are what the predicate admitted the
    /// lease with. A lease charged flat reads the same at every slot.
    ///
    /// [`Scheduler::load_at`]: crate::Scheduler::load_at
    /// [`Scheduler::charge_at`]: crate::Scheduler::charge_at
    fn scored<'a>(
        adm: &mut Admission,
        workers: &BTreeMap<WorkerId, Worker>,
        admitted: &[Passed<'a>],
        attempt: &Attempt<'_>,
        now: FarmTime,
    ) -> Option<&'a WorkerId> {
        let request = attempt.request;
        let shared: Vec<&Passed<'a>> = admitted
            .iter()
            .filter(|p| !workers[p.name].exclusive(request))
            .collect();
        match shared.as_slice() {
            [] => return least_loaded(&Self::loads(admitted)),
            [only] => return Some(only.name),
            _ => {}
        }
        let hosts = shared
            .iter()
            .map(|p| {
                let w = &workers[p.name];
                let facts = HostFacts {
                    cpu_capacity_millis: 0,
                    memory_capacity_bytes: 0,
                    fit_minutes: attempt.fit_minutes,
                    fallback: p.left.fallback,
                    measured_above_predicted: w.load_state.measured_above_covered(),
                    same_key_millis: w.load_state.same_key_millis(attempt.cost.key_hash),
                    new_key_millis: 0,
                    uncovered_leases: w.load_state.uncovered_leases(),
                };
                // The figures the predicate charges at every slot; slot 0 is what it
                // admitted the lease with (`p.left`, `p.cost`).
                let mut left = [Charge::default(); SLOTS];
                let mut new = [Charge::default(); SLOTS];
                for at in 0..SLOTS {
                    let (cpu_millis, memory_bytes) =
                        w.load_state.left_at(at, now, &w.capacity, &adm.cfg);
                    left[at] = Charge {
                        cpu_millis,
                        memory_bytes,
                    };
                    let (cpu_millis, memory_bytes) = charge_on(
                        w,
                        request,
                        attempt.cost,
                        attempt.profile.as_ref(),
                        &p.left,
                        at,
                    );
                    new[at] = Charge {
                        cpu_millis,
                        memory_bytes,
                    };
                }
                (
                    p.name.clone(),
                    SlotHost::new(&w.capacity, left, new, p.cost.cpu_millis, facts),
                )
            })
            .collect();
        let model = SlotLoad {
            lease: LeaseFacts {
                lease: attempt.lease,
                cold: attempt.cost.cold,
                retry: attempt.cost.retry,
            },
            hosts,
        };
        let names: Vec<&'a WorkerId> = shared.iter().map(|p| p.name).collect();
        let mut scorer = adm.scorer();
        let choice = scorer.choose_among(&model, &names)?;
        adm.count_scored(scorer.evaluations(), &choice);
        Some(choice.worker)
    }

    /// The cost `attempt` is charged on `worker`, and what the worker is charged before it.
    fn charged(
        adm: &Admission,
        worker: &Worker,
        attempt: &Attempt<'_>,
        now: FarmTime,
    ) -> (LeaseCost, Left) {
        let cost = match attempt.request.kind {
            LeaseKind::Action => {
                worker.charged_whole(attempt.request, clamped(attempt.cost, &worker.capacity))
            }
            // A VM is charged what it books; a whole-machine lease is not on the ledger.
            LeaseKind::Vm | LeaseKind::WholeMachine => attempt.cost,
        };
        let left = worker.load_state.left(now, &worker.capacity, &adm.cfg);
        (cost, left)
    }

    /// The worker a blocked head of the queue holds: of the workers `request` could run
    /// on but for the load, not in `reserved` or skipped for `avoid` and not owned whole
    /// by the request, the one it can start on first ([`start_time`], with the lease ends
    /// in `ends`), a worker with no start time after every worker with one; among equals
    /// the smallest binding ratio, then the first by name. Nothing is kept from the last
    /// round. `None` when no worker could take it at all.
    ///
    /// Two choices are deliberate. Ties among workers whose start time is unknown or equal
    /// use the binding ratio, which keeps the order the hold had before start times. And
    /// the start time is projected against the flat predicate (CPU and memory caps), not
    /// the horizon curves, the flat-memory guard or the cold cap: those can only make the
    /// real start later, so the projected start is a lower bound, and a backfill gate that
    /// admits only work ending by it admits only work that also ends before the real
    /// start. It costs some backfill and never starves the head.
    pub(crate) fn hold_for_head(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        adm: &Admission,
        attempt: &Attempt<'_>,
        ends: &BTreeMap<LeaseId, Option<u64>>,
    ) -> Option<WorkerId> {
        let request = attempt.request;
        let at = self.matching(workers, request);
        let skip = self.avoided(at, workers, request, attempt.avoid);
        let now = self.now;
        let held: Vec<(&WorkerId, (Option<u64>, Ratio))> = self.memo[at]
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
                // The charge of the new lease at the present minute.
                let profile = Self::profile_on(w, attempt);
                let (new_cpu, new_mem) = profile.map_or_else(
                    || (left.charged_cpu(&cost), cost.memory_bytes),
                    |p| {
                        (
                            p.new_cpu(0).min(w.capacity.cpu_millis),
                            p.new_mem(0).min(w.capacity.memory_bytes),
                        )
                    },
                );
                let ratio = binding(
                    (left.cpu.saturating_add(new_cpu), w.capacity.cpu_millis),
                    (left.mem.saturating_add(new_mem), w.capacity.memory_bytes),
                );
                let charges = w.load_state.charges(&left);
                let limits = (
                    adm.cfg.cpu_limit(&w.capacity),
                    adm.cfg.mem_limit(&w.capacity),
                );
                let start = start_time(
                    &left,
                    &charges,
                    ends,
                    limits,
                    (new_cpu, new_mem),
                    now.as_millis(),
                );
                (name, (start, ratio))
            })
            .collect();
        // The first of the least: `held` is in name order.
        held.into_iter()
            .min_by_key(|(_, (start, ratio))| (start.is_none(), *start, *ratio))
            .map(|(name, _)| name.clone())
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
            w.capacity.fits(&request.demand())
                && w.capacity.memory_bytes > request.resources.memory_bytes
        })
    }

    /// The worker to hold for a whole-machine `request` that fits nowhere now: the first
    /// of `drain` (the workers by drain cost, the least tail first) that could run it, with
    /// no preference for a worker held before. Never one in `reserved`,
    /// nor one in `avoid` while [`Servable::fit`] skips it. Only a whole-machine lease or an
    /// action that owns its worker ([`Request::owns_worker`]) holds a worker (the caller
    /// asks only for one); an action at a worker's whole memory waits until some worker
    /// has no lease, and nothing reserves one for it.
    pub(crate) fn reserve(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
        reserved: &BTreeSet<WorkerId>,
        avoid: &BTreeSet<WorkerId>,
        drain: &[DrainCost],
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
                    && workers[*name].capacity.fits(&request.demand())
            })
            .collect();
        drain
            .iter()
            .find_map(|cost| candidates.iter().find(|name| name.as_str() == cost.id))
            .map(|name| (*name).clone())
    }

    /// The live workers placement may use that could run `request` once their bookings
    /// end and that are not in `reserved`: where an ordinary request that waits could go.
    pub(crate) fn capable(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
        reserved: &BTreeSet<WorkerId>,
    ) -> Vec<&WorkerId> {
        let at = self.matching(workers, request);
        self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .filter(|name| {
                !reserved.contains(*name) && workers[*name].capacity.fits(&request.demand())
            })
            .collect()
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
                !avoid.contains(name) && workers[name].capacity.fits(&request.demand())
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
                    && w.capacity.fits(&request.demand())
            })
            .collect();
        if could.is_empty() {
            return verdict;
        }
        let workers = could
            .into_iter()
            .map(|(name, phase)| (name.clone(), phase.clone()))
            .collect();
        Verdict::Cordoned(Unservable::Cordoned { workers })
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
                    && w.capacity.fits(&request.demand())
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
                Unservable::NoWorker
            } else if self
                .cordoned
                .iter()
                .all(|name| workers[name].phase == NodePhase::Serving)
            {
                Unservable::EveryWorkerCordoned
            } else {
                Unservable::EveryWorkerHeldBack
            };
            return Verdict::Unservable(why);
        }
        let kind = request.kind;
        let serving = self
            .live
            .iter()
            .filter(|name| serves(&workers[*name].caps, kind))
            .count();
        if serving == 0 {
            return Verdict::Unservable(Unservable::NoDriver {
                live: self.live.len(),
                kind,
            });
        }
        let at = self.matching(workers, request);
        let matching = &self.memo[at].1;
        if matching.is_empty() {
            let (closest, unmet) = self
                .live
                .iter()
                .filter(|name| serves(&workers[*name].caps, kind))
                .map(|name| (name, request.needs.unmet(&workers[name].caps)))
                .min_by_key(|(_, unmet)| unmet.len())
                .expect("there is a live worker");
            let unmet: Vec<String> = unmet.iter().map(ToString::to_string).collect();
            return Verdict::Unservable(Unservable::PlatformMismatch {
                serving,
                some_cannot_serve: serving != self.live.len(),
                kind,
                closest: closest.clone(),
                lacks: unmet,
            });
        }
        let large_enough = matching
            .iter()
            .any(|&i| workers[&self.live[i]].capacity.fits(&request.demand()));
        if large_enough {
            return Verdict::Servable;
        }
        Verdict::Unservable(Unservable::TooLarge {
            matching: matching.len(),
            asked: request.resources,
        })
    }
}
