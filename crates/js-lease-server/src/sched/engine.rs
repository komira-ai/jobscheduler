//! The one `js-sched` scheduler a [`super::place::SchedPlacer`] keeps for the life of the
//! server, and the picture of the registry's books it keeps beside it.
//!
//! The engine learns the registry's books from the registry's own events, not from a
//! snapshot per start: a host that says hello is a worker up ([`Engine::host_up`]), with
//! the capacity and caps its pool's shape gives ([`map::host`]); an accepted heartbeat is
//! a heartbeat, with the reading the registry vetted and converted ([`Engine::heard`]); a
//! lease the placer places is booked by the placement itself ([`Engine::grant`]), and one
//! the registry stops is released ([`Engine::ended`]).
//!
//! At each start the two pictures are compared on the hosts the start could book
//! ([`Engine::agrees`]): every one must be a worker of the same shape, holding exactly the
//! leases the registry books there. When they disagree (a lease the registry booked that
//! the engine never placed, as one put back after a restart; one the engine gave up for a
//! silent host; a host it was never told of) the engine is built again from the
//! registry's view of every host it lists ([`Engine::build`]); that is the only way it is
//! ever built again.
//!
//! The engine places a start only on the hosts the registry offered it
//! ([`Scheduler::place_only_on`]); once the start is booked, the operation is held to no
//! host, so a lease the engine gives up (its host silent past the engine's lease grace)
//! is never placed again by the engine alone: it waits, and the next start that meets
//! that host rebuilds the engine. Every host the registry offers is heard from at that
//! start ([`Event::Capacity`]), so the registry, not the engine's own grace, says which
//! hosts are live.

use std::collections::{BTreeMap, BTreeSet};

use js_caps::NodeCaps;
use js_pool_static::Candidate;
use js_provider::StartSpec;
use js_registry::MarkedTails;
use js_sched::harness::feed;
use js_sched::{
    Backfill, DaemonInstance, Event, LoadConfig, MemoryPercent, OpState, Prediction, Recovered,
    RecoveredLease, Request, Scheduler,
};
use js_types::sched::usage::{LeaseClock, LeaseCost, LeaseMinute, NodeLoad};
use js_types::sched::{
    Effect, FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, WaiterId, WorkKey,
};
use js_types::{HostId, LeaseId, Shape};

use super::map::{self, MapError};

/// The term the engine grants leases in. A lease it places is numbered `TERM-<seq>`, with
/// the sequence of the registry's lease, so the tie-break between equal hosts (a hash of
/// the lease and the host) varies by lease as it does in the registry. A lease it takes
/// up when it is built is numbered in term 0.
const TERM: u64 = 1;

/// How the engine is built: the settings of the server's configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Whether a start may backfill a host set aside, and up to which wall time.
    pub(crate) backfill: Backfill,
    /// The load settings, CPU slack included (`--cpu-slack`).
    pub(crate) load: LoadConfig,
    /// The share of each host's memory the engine may book (`memory_schedulable_percent`).
    pub(crate) memory: MemoryPercent,
}

/// The scheduler form of a host: its capacity and the capabilities its node report gives.
pub(crate) type Node = (js_types::sched::Resources, NodeCaps);

/// A lease of the registry the engine holds.
#[derive(Debug, Clone, Copy)]
struct Placed {
    /// The engine's lease.
    lease: SchedLeaseId,
    /// The engine's operation.
    operation: OperationId,
}

/// The scheduler and what ties it to the registry's books.
#[derive(Debug)]
pub(crate) struct Engine {
    sched: Scheduler,
    /// Every host the engine was told of, with the shape the registry books it as.
    hosts: BTreeMap<HostId, Shape>,
    /// The registry's leases on each host, as the engine placed or took them up.
    on: BTreeMap<HostId, BTreeMap<LeaseId, Placed>>,
    /// The registry's lease of each engine lease.
    back: BTreeMap<SchedLeaseId, (HostId, LeaseId)>,
    /// The hosts held for a whole-node request outside the engine.
    outside: BTreeSet<HostId>,
    /// The farm time last fed.
    now: FarmTime,
}

impl Engine {
    /// An engine built with `settings` over `every`, the hosts the registry lists with
    /// the leases on each, at `now`. Every host with a scheduler form is a worker; every
    /// lease is taken up on its host, as a lease recovered after a restart is.
    pub(crate) fn build(settings: Settings, every: &[Candidate], now: FarmTime) -> Self {
        let mut sched = Scheduler::new(TERM)
            .with_backfill(settings.backfill)
            .with_load_config(settings.load)
            .with_memory_percent(settings.memory);
        let held = every
            .iter()
            .flat_map(|candidate| candidate.leases.iter().map(move |held| (candidate, held)));
        let mut operations = Vec::new();
        let mut holds = Vec::new();
        for (number, (candidate, held)) in (0..).zip(held) {
            operations.push(recovered(number, &candidate.host, held));
            let placed = Placed {
                lease: SchedLeaseId::new(0, number),
                operation: OperationId(number),
            };
            holds.push((candidate.host.clone(), held.lease, placed));
        }
        let next = OperationId(u64::try_from(operations.len()).unwrap_or(u64::MAX));
        sched.recover(TERM, next, 0, operations);
        let mut engine = Self {
            sched,
            hosts: BTreeMap::new(),
            on: BTreeMap::new(),
            back: BTreeMap::new(),
            outside: BTreeSet::new(),
            now,
        };
        for candidate in every {
            engine.host_up(&candidate.host, &candidate.shape, now);
        }
        for (host, lease, placed) in holds {
            engine.hold(&host, lease, placed);
        }
        engine
    }

    /// Feeds `event` at `now`, or at the last time fed if that is later.
    fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Effect> {
        self.now = self.now.max(now);
        feed(&mut self.sched, self.now, event)
    }

    /// Records that the engine holds `lease` of the registry on `host` as `placed`, and
    /// holds its operation to no host: if the engine gives the lease up, it never places
    /// it again by itself.
    fn hold(&mut self, host: &HostId, lease: LeaseId, placed: Placed) {
        self.sched.place_only_on(placed.operation, BTreeSet::new());
        self.on
            .entry(host.clone())
            .or_default()
            .insert(lease, placed);
        self.back.insert(placed.lease, (host.clone(), lease));
    }

    /// `host` said hello, booked as `shape`: a worker up, of the capacity and caps the
    /// shape gives. A shape with no scheduler form is remembered, and the host is no
    /// worker: a start offered it falls back ([`Engine::nodes`]).
    pub(crate) fn host_up(&mut self, host: &HostId, shape: &Shape, now: FarmTime) {
        if let Ok((capacity, caps)) = map::host(shape) {
            let up = Event::WorkerUp {
                worker: map::worker(host),
                instance: DaemonInstance::new(host.as_str()),
                predecessor_stopped: false,
                capacity,
                caps,
            };
            self.feed(now, up);
        }
        self.hosts.insert(host.clone(), shape.clone());
    }

    /// A heartbeat of `host` the registry accepted, with the reading it brought. The
    /// heartbeat lists every lease the engine holds there as running, so none is given up
    /// for a beat; the reading's leases are named as the engine numbers them, and a lease
    /// the engine does not hold is left out of it. The scheduler takes no heartbeat of a
    /// host that is no worker of it.
    pub(crate) fn heard(&mut self, host: &HostId, load: Option<&NodeLoad>, now: FarmTime) {
        let here = self.on.get(host);
        let ours = |lease: &SchedLeaseId| {
            let registry = LeaseId::new(lease.term, lease.seq);
            here.and_then(|leases| leases.get(&registry))
                .map(|placed| placed.lease)
        };
        let running = here
            .into_iter()
            .flat_map(|leases| leases.values().map(|placed| placed.lease))
            .collect();
        let load = load.map(|load| NodeLoad {
            covered: load.covered.iter().filter_map(ours).collect(),
            lease_clocks: load
                .lease_clocks
                .iter()
                .filter_map(|clock| ours(&clock.lease).map(|lease| clock_of(clock, lease)))
                .collect(),
            minutes: load
                .minutes
                .iter()
                .filter_map(|minute| ours(&minute.lease).map(|lease| minute_of(minute, lease)))
                .collect(),
            ..load.clone()
        });
        let beat = Event::Heartbeat {
            worker: map::worker(host),
            running,
            load,
        };
        self.feed(now, beat);
    }

    /// The registry stopped `lease`: the engine releases it, whether it still holds it or
    /// gave it up and waits with it. A lease the engine does not know changes nothing.
    pub(crate) fn ended(&mut self, lease: LeaseId) {
        let found = self
            .on
            .iter_mut()
            .find_map(|(_, leases)| leases.remove(&lease));
        let Some(placed) = found else {
            return;
        };
        self.back.remove(&placed.lease);
        let queued = self.sched.unplace(placed.lease).unwrap_or(placed.operation);
        let _ = self.sched.forget_queued(queued);
    }

    /// Whether the engine's picture agrees with the registry's on `open`, the hosts a
    /// start numbered `sequence` could book: each is a worker of the shape the registry
    /// books it as, and holds exactly the registry's leases there, none given up; and the
    /// lease the engine would grant for the start is not one it holds.
    pub(crate) fn agrees(&self, open: &[Candidate], sequence: u64) -> bool {
        let fresh = !self.back.contains_key(&SchedLeaseId::new(TERM, sequence));
        fresh && open.iter().all(|candidate| self.agrees_on(candidate))
    }

    fn agrees_on(&self, candidate: &Candidate) -> bool {
        let same = self.hosts.get(&candidate.host) == Some(&candidate.shape);
        let empty = BTreeMap::new();
        let here = self.on.get(&candidate.host).unwrap_or(&empty);
        let worker = map::worker(&candidate.host);
        let held = |placed: &Placed| {
            let leased = OpState::Leased {
                lease: placed.lease,
                worker: worker.clone(),
                committed: true,
            };
            self.sched.state(placed.operation) == Some(&leased)
        };
        let listed = candidate.leases.len() == here.len()
            && candidate
                .leases
                .iter()
                .all(|booked| here.get(&booked.lease).is_some_and(held));
        same & listed
    }

    /// The scheduler form of each host of `open`, in its order.
    ///
    /// # Errors
    /// [`MapError`] of the first host that has none.
    pub(crate) fn nodes(open: &[Candidate]) -> Result<Vec<Node>, MapError> {
        open.iter()
            .map(|candidate| map::host(&candidate.shape))
            .collect()
    }

    /// Places `spec` on one of `open`, the hosts the registry offers it, as the engine
    /// decides: the index of the host, or `None` when the engine places it nowhere. A host
    /// in `marked` is held for the request that set it aside, until the end of its tail,
    /// and takes the start only as backfill. The engine is assumed to agree with the
    /// registry on `open` ([`Engine::agrees`]).
    ///
    /// The start is a submitted request held to `open`, charged its CPU and memory as it
    /// books them, with the max wall time the lease table enforces; one placement round
    /// later the engine has placed it or not. Placed, it is booked in the engine as the
    /// registry books it; not placed, it is forgotten.
    ///
    /// # Errors
    /// [`MapError`] when the request has no scheduler form.
    pub(crate) fn grant(
        &mut self,
        spec: &StartSpec,
        (open, nodes): (&[Candidate], Vec<Node>),
        marked: &MarkedTails,
        now: FarmTime,
    ) -> Result<Option<usize>, MapError> {
        let mut request = map::job(spec.request(), &spec.lease().to_string())?;
        request.whole_worker = spec.class().admission().books_whole_host();
        // Every host offered is live, the registry says, and is heard from now; `nodes`
        // are their scheduler forms ([`Engine::nodes`]).
        for (candidate, (capacity, caps)) in open.iter().zip(nodes) {
            let worker = map::worker(&candidate.host);
            let heard = Event::Capacity {
                worker,
                capacity,
                caps,
            };
            self.feed(now, heard);
        }
        self.mark(marked);
        let pin = request.pin.clone();
        // A probe's host is shown only once it was admitted there: it is cordoned, for
        // this start only.
        if let Some(pin) = &pin {
            self.feed(
                now,
                Event::Cordon {
                    worker: pin.clone(),
                },
            );
        }
        let operation = self.sched.next_operation();
        let lease = spec.lease();
        self.sched
            .recover(TERM, operation, lease.sequence(), Vec::new());
        // The lease table ends the lease at its wall time: the end backfill reads.
        self.sched.set_max_wall(operation, spec.request().max_wall);
        let resources = &spec.request().resources;
        let charge = Prediction {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            cold: false,
            life_minutes: 1,
        };
        let only = open.iter().map(|c| map::worker(&c.host)).collect();
        let submit = Event::Submit {
            waiter: WaiterId(operation.0),
            request,
        };
        self.feed(now, submit);
        self.sched.set_cost(operation, charge);
        self.sched.place_only_on(operation, only);
        self.feed(now, Event::Tick);
        let granted = match self.sched.state(operation) {
            Some(OpState::Leased { lease, worker, .. }) => Some((*lease, worker.clone())),
            _ => None,
        };
        let at = granted.and_then(|(granted, worker)| {
            let on = open.iter().position(|c| map::worker(&c.host) == worker);
            on.map(|at| (at, granted))
        });
        match at {
            Some((index, granted)) => {
                let placed = Placed {
                    lease: granted,
                    operation,
                };
                self.hold(&open[index].host, lease, placed);
            }
            None => {
                let _ = self.sched.forget_queued(operation);
            }
        }
        if let Some(pin) = pin {
            self.feed(now, Event::Uncordon { worker: pin });
        }
        Ok(at.map(|(index, _)| index))
    }

    /// Holds the hosts of `marked` outside the engine, each until the end of its tail as
    /// the mark took it, and ends the hold of a host no longer marked.
    fn mark(&mut self, marked: &MarkedTails) {
        let gone: Vec<HostId> = self
            .outside
            .iter()
            .filter(|host| !marked.contains_key(*host))
            .cloned()
            .collect();
        for host in gone {
            self.sched.release_outside(&map::worker(&host));
            self.outside.remove(&host);
        }
        for (host, left) in marked {
            let end = left.map(|left| millis(left).saturating_add(self.now.as_millis()));
            let start = end.map(FarmTime::from_millis);
            self.sched.reserve_outside(map::worker(host), start);
            self.outside.insert(host.clone());
        }
    }
}

/// The recovered operation, numbered `number`, that stands for the lease `held` on
/// `host`: charged its CPU and memory as it books them.
fn recovered(number: u64, host: &HostId, held: &js_pool_static::Held) -> Recovered {
    let booked = map::booking(&held.resources);
    let request = Request {
        key: WorkKey::new("held", held.lease.to_string()),
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: booked,
        needs: js_caps::Request::default(),
        hermetic: false,
        do_not_cache: true,
        whole_worker: held.whole,
        pin: None,
    };
    let cost = LeaseCost {
        cpu_millis: booked.cpu_millis,
        memory_bytes: booked.memory_bytes,
        limit_bytes: booked.memory_bytes,
        cold: false,
        retry: false,
        key_hash: 0,
    };
    Recovered {
        operation: OperationId(number),
        request,
        waiter: WaiterId(number),
        lease: Some(RecoveredLease {
            lease: SchedLeaseId::new(0, number),
            worker: map::worker(host),
            cost: Some(cost),
        }),
        losses: 0,
        fate: None,
    }
}

/// `clock` of the engine's `lease`.
fn clock_of(clock: &LeaseClock, lease: SchedLeaseId) -> LeaseClock {
    LeaseClock { lease, ..*clock }
}

/// `minute` of the engine's `lease`.
fn minute_of(minute: &LeaseMinute, lease: SchedLeaseId) -> LeaseMinute {
    LeaseMinute { lease, ..*minute }
}

/// `duration` in milliseconds, saturating.
fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use js_pool_static::Held;
    use js_types::sched::WorkerId;
    use js_types::sched::usage::{LeaseClock, LeaseMinute};
    use js_types::{Resources, Shape};

    use super::*;

    /// A shape of 8 cores and 16 GiB, with an `arch` when `arch` says so.
    fn shape(arch: bool) -> Shape {
        let caps = arch
            .then(|| ("arch".to_owned(), "x86_64".to_owned()))
            .into_iter()
            .collect();
        Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
    }

    /// `name` booked as `shape`, carrying the one-core leases `1-<seq>` of `seqs`.
    fn candidate(name: &str, shape: Shape, seqs: &[u64]) -> Candidate {
        let leases = seqs.iter().map(|seq| Held {
            lease: LeaseId::new(1, *seq),
            resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
            whole: false,
        });
        Candidate {
            host: HostId::new(name).unwrap(),
            shape,
            leases: leases.collect(),
        }
    }

    fn at(ms: u64) -> FarmTime {
        FarmTime::from_millis(ms)
    }

    /// A reading numbered `seq` naming the registry's leases `leases` as covered, with a
    /// clock and a minute of each.
    fn reading(seq: u64, leases: &[SchedLeaseId]) -> NodeLoad {
        NodeLoad {
            seq,
            mono_usec: seq * 5_000_000,
            cpu_usage_usec: seq * 5_000_000,
            memory_working_set_bytes: 1 << 30,
            covered: leases.to_vec(),
            lease_clocks: leases
                .iter()
                .map(|lease| LeaseClock {
                    lease: *lease,
                    started_mono_usec: 0,
                })
                .collect(),
            minutes: leases
                .iter()
                .map(|lease| LeaseMinute {
                    lease: *lease,
                    minute: 0,
                    cpu_mean_millis: 500,
                    cpu_peak_window_millis: 900,
                    ws_max_bytes: 1 << 20,
                })
                .collect(),
            ..NodeLoad::default()
        }
    }

    /// Catches: a host whose shape has no scheduler form made a worker anyway, or its
    /// leases dropped from the picture. Built over `a` (no `arch`) and `b`, the engine has
    /// a worker for `b` alone, holding `b`'s lease as the second it took up, and still
    /// records `a`'s lease on `a`.
    #[test]
    fn a_host_with_no_scheduler_form_is_no_worker() {
        let every = [
            candidate("a", shape(false), &[1]),
            candidate("b", shape(true), &[2]),
        ];
        let engine = Engine::build(Settings::default(), &every, at(0));
        assert!(engine.sched.capacity(&WorkerId::new("a")).is_none());
        assert!(engine.sched.capacity(&WorkerId::new("b")).is_some());
        let b = HostId::new("b").unwrap();
        assert_eq!(
            engine.sched.leases_on(&map::worker(&b)),
            [SchedLeaseId::new(0, 1)]
        );
        let a = HostId::new("a").unwrap();
        assert_eq!(engine.on[&a].len(), 1);
        assert!(Engine::nodes(&every).is_err());
        assert_eq!(Engine::nodes(&every[1..]).map(|nodes| nodes.len()), Ok(1));
    }

    /// Catches: a reading's leases passed in the registry's numbering (the engine would
    /// see none of its own covered and charge each lease again on top of the working set)
    /// and a lease the engine does not hold kept in it. After two readings that cover the
    /// registry's lease `1-2`, the engine counts it covered; readings that name only a
    /// lease it does not hold leave its lease uncovered.
    #[test]
    fn a_reading_names_the_engines_own_leases() {
        let every = [candidate("b", shape(true), &[2])];
        let b = HostId::new("b").unwrap();
        let worker = map::worker(&b);
        let ours = SchedLeaseId::new(1, 2);
        let other = SchedLeaseId::new(7, 7);
        let mut covered = Engine::build(Settings::default(), &every, at(0));
        let mut stray = Engine::build(Settings::default(), &every, at(0));
        for seq in 1..=2 {
            let now = at(seq * 5_000);
            covered.heard(&b, Some(&reading(seq, &[ours, other])), now);
            stray.heard(&b, Some(&reading(seq, &[other])), now);
        }
        let view = |engine: &Engine| engine.sched.load_view(&worker).unwrap();
        assert_eq!(view(&covered).uncovered_cpu_millis, 0);
        assert_eq!(view(&stray).uncovered_cpu_millis, 1000);
        assert!(!view(&covered).fallback);
    }

    /// Catches: a heartbeat of a host the engine was never told of, or the stop of a lease
    /// it never held, changing what it holds or panicking.
    #[test]
    fn what_the_engine_does_not_know_changes_nothing() {
        let every = [candidate("b", shape(true), &[2])];
        let mut engine = Engine::build(Settings::default(), &every, at(0));
        let stranger = HostId::new("z").unwrap();
        engine.heard(&stranger, Some(&reading(1, &[])), at(1));
        engine.heard(&stranger, None, at(2));
        engine.ended(LeaseId::new(5, 5));
        let b = HostId::new("b").unwrap();
        assert_eq!(engine.sched.leases_on(&map::worker(&b)).len(), 1);
        assert_eq!(engine.on.len(), 1);
        assert!(engine.agrees(&every, 3));
    }
}
