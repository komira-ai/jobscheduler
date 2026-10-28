//! A reference placement for placement by load, written from `docs/design/placement-by-load.md`
//! and calling nothing in `js-sched`: the fleet simulations (F1, F3, F4) hold the
//! scheduler's grants to it, in order, the way they hold the booked rule to theirs.
//!
//! It is a direct, slow reading of the document for the worlds those simulations build:
//!
//! - **Flat charges only.** No simulated action has a learned cost, so each is charged
//!   what it asks for in cores (cut to the worker's cores) and no memory, at every slot of
//!   the horizon. Every figure the scorer reads is then the same at every slot, so each
//!   sum over slots is the weight of the slots times one residual: [`SLOT_WEIGHTS`] and
//!   [`FIT_WEIGHTS`].
//! - **No worker measures.** The simulated workers send no reading, so every one is charged
//!   by prediction alone (Admission, Fallback): the sum of the predicted cores of all its
//!   leases, each at least [`UNMEASURED_FLOOR_MILLIS`] cores' thousandths (within the
//!   worker's cores), because nothing there measures a light population. The score's
//!   third key (a worker in fallback) is then the same for every candidate and orders
//!   nothing, and its first tie-break counts every lease of a worker as uncovered.
//! - **No lease cap.** A host built with a cap is refused ([`Host::new`]).
//!
//! Memory is never booked against a worker for an action that has no learned cost, so the
//! memory side of every sum is 0 and a worker's memory enters the score as a constant
//! full leftover; the request must still be one the worker's memory can hold, and an
//! action at the whole host's memory runs alone.
//!
//! The settings are the document's defaults: slack 10%, score tolerance 1% of capacity,
//! key share 25%, balance 1.

#![allow(
    dead_code,
    reason = "each test binary uses some of the reference, and its own tests use the rest"
)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use js_types::sched::{OperationId, Resources, WorkKey, WorkerId};

/// `s_cpu`, in thousandths: planned CPU stops at `C (1 + s_cpu)`.
pub const SLACK_PERMILLE: u128 = 100;
/// The score tolerance, in parts per million of the worker's own capacity.
pub const TOLERANCE_PPM: u128 = 10_000;
/// The share of a worker's cores one key may hold before the score treats the worker as
/// concentrated on it, in parts per million.
pub const KEY_SHARE_PPM: u128 = 250_000;
/// `--placement-score-balance`.
pub const BALANCE: u128 = 1;
/// The least a lease is charged on a worker that never sent a reading, in millicores.
pub const UNMEASURED_FLOOR_MILLIS: u64 = 1_000;
/// The minutes of the horizon `H`.
pub const HORIZON: u128 = 30;
const PPM: u128 = 1_000_000;

/// The weights of the slots the safety keys read: `w(tau) = H + 1 - tau` for `tau` in
/// `0..=H` (31 down to 1) and 1 for the `H+` slot. For a flat charge every slot reads the
/// same figure, so a weight total only scales a key: it cannot change how two workers
/// compare, and is here to say what the document's sum is.
pub const SLOT_WEIGHTS: u128 = (HORIZON + 1) * (HORIZON + 2) / 2 + 1;
/// The weights of the slots the fit reads: a flat lease has `L = 1`, so only `tau = 0`, of
/// weight `H + 1`.
pub const FIT_WEIGHTS: u128 = HORIZON + 1;

/// A worker as one placement round finds it.
#[derive(Clone, Debug)]
pub struct Host {
    pub name: WorkerId,
    pub capacity: Resources,
    /// GPUs and VM slots booked by its leases (the axes the load predicate does not sum).
    pub booked_gpus: u64,
    pub booked_vms: u64,
    /// One lease holds the node alone.
    pub whole: bool,
    /// Each lease it holds, as the action key and the cores it was charged when placed.
    leases: Vec<(WorkKey, u64)>,
    /// The cores it is charged: the sum over `leases`, each at least the floor.
    charged: u64,
}

impl Host {
    pub fn new(
        name: WorkerId,
        capacity: Resources,
        booked_gpus: u64,
        booked_vms: u64,
        whole: bool,
        leases: Vec<(WorkKey, u64)>,
    ) -> Self {
        assert!(
            capacity.leases.is_none(),
            "the reference does not model a lease cap"
        );
        let mut host = Self {
            name,
            capacity,
            booked_gpus,
            booked_vms,
            whole,
            leases: Vec::new(),
            charged: 0,
        };
        for (key, cpu) in leases {
            host.charged += cpu.max(host.floor());
            host.leases.push((key, cpu));
        }
        host
    }

    /// The least a lease is charged here, in millicores.
    fn floor(&self) -> u64 {
        UNMEASURED_FLOOR_MILLIS.min(self.capacity.cpu_millis)
    }

    /// The cores a lease of `request` is charged here: what it asks for, cut to the
    /// worker's cores. This is what the host remembers of it.
    pub fn charge_of(&self, request: &Resources) -> u64 {
        request.cpu_millis.min(self.capacity.cpu_millis)
    }

    /// `request` is an action at the whole host's memory: it runs alone.
    fn owns_whole(&self, request: &Resources) -> bool {
        request.memory_bytes > 0 && request.memory_bytes >= self.capacity.memory_bytes
    }

    /// The cores the host is charged before a new lease: every lease, at least the floor.
    pub fn left(&self) -> u64 {
        self.charged
    }

    /// What `request` adds to `left`: its charge, at least the floor.
    pub fn new_charge(&self, request: &Resources) -> u64 {
        self.charge_of(request).max(self.floor())
    }

    /// The most the host is planned to: `C_cpu (1 + s_cpu)`, rounded down.
    pub fn cpu_limit(&self) -> u64 {
        u64::try_from(u128::from(self.capacity.cpu_millis) * (1_000 + SLACK_PERMILLE) / 1_000)
            .expect("fits")
    }

    /// Whether `request` may be placed here but for the load: nothing holds the node
    /// alone, its GPUs and VM slots are free, the cores and memory are ones the worker
    /// has, and an action at the whole host's memory finds the worker without a lease.
    pub fn has_room(&self, request: &Resources) -> bool {
        !self.whole
            && request.gpus <= self.capacity.gpus.saturating_sub(self.booked_gpus)
            && request.vms <= self.capacity.vms.saturating_sub(self.booked_vms)
            && request.cpu_millis <= self.capacity.cpu_millis
            && request.memory_bytes <= self.capacity.memory_bytes
            && (!self.owns_whole(request) || self.leases.is_empty())
    }

    /// Whether the load predicate admits `request`: the cores charged plus its own stay
    /// within the limit. (The memory side is 0 = 0 for a lease with no learned cost.)
    pub fn within_limits(&self, request: &Resources) -> bool {
        self.left() + self.new_charge(request) <= self.cpu_limit()
    }

    /// Whether `request` may be placed here under the load rule.
    pub fn admits(&self, request: &Resources) -> bool {
        self.has_room(request) && self.within_limits(request)
    }

    /// Books a lease of `key` and `request` on the host.
    pub fn book(&mut self, key: &WorkKey, request: &Resources) {
        self.whole |= self.owns_whole(request);
        self.booked_gpus += request.gpus;
        self.booked_vms += request.vms;
        let charge = self.charge_of(request);
        self.charged += charge.max(self.floor());
        self.leases.push((key.clone(), charge));
    }
}

/// One queued request, as a round meets it.
pub struct Ask<'a> {
    pub operation: OperationId,
    pub key: &'a WorkKey,
    pub resources: Resources,
    /// A number shared by every request that runs on the same set of workers (platform
    /// and kind): one head holds one worker per pool.
    pub pool: usize,
    /// The workers (indices into the round's hosts) it may use at all: live, serving, not
    /// cordoned, satisfying its platform, and not one its earlier runs ended on while
    /// another could take it.
    pub candidates: &'a [usize],
}

/// The key a candidate is ranked by; lower is better, compared field by field. The third
/// key of the document (a worker in fallback) is left out: it is the same for every host.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    /// 1: the weighted overshoot past 100% of the cores, beyond the tolerance.
    slack: u128,
    /// 2: a memory miss would not fit. Always 0 here: a lease with no learned cost
    /// charges no memory.
    memory_unsafe: bool,
    /// 4: the lease's key would take more than its share of the worker's cores.
    concentrated: bool,
    /// 5: the leftover of the binding resource squared, and the stranded resource.
    fit: u128,
    /// Tie-break 1: fewer leases no reading covers (every lease of a worker that sent no
    /// reading).
    uncovered: usize,
    /// Tie-break 2: the larger worker.
    larger: Reverse<u64>,
    /// Tie-break 3: the larger headroom over the next five minutes.
    headroom: Reverse<i128>,
    /// Tie-break 4: the hash of lease and worker.
    hash: u64,
    /// Tie-break 5.
    name: WorkerId,
}

/// The leftover `x` and the overshoot `o`, in parts per million of `capacity`, of
/// `capacity - load - new`: floor division, one of the two is 0, a capacity of 0 counts as 1.
fn residual(capacity: u64, load: u64, new: u64) -> (u128, u128) {
    let capacity = capacity.max(1);
    let spare = i128::from(capacity) - i128::from(load) - i128::from(new);
    let ppm = spare.unsigned_abs() * PPM / u128::from(capacity);
    if spare >= 0 { (ppm, 0) } else { (0, ppm) }
}

/// The 64-bit FNV-1a hash of `bytes`.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The last tie-break: FNV-1a of the lease id's `term-seq` text and the worker id, each
/// preceded by its length as a `u32` in little-endian bytes.
pub fn tie_hash(term: u64, seq: u64, worker: &WorkerId) -> u64 {
    let lease = format!("{term}-{seq}");
    let mut bytes = Vec::new();
    for part in [lease.as_bytes(), worker.as_str().as_bytes()] {
        bytes.extend_from_slice(&u32::try_from(part.len()).expect("short").to_le_bytes());
        bytes.extend_from_slice(part);
    }
    fnv1a(&bytes)
}

/// `a / b` against `c / d` for positive `b` and `d`.
fn share_cmp(a: (u64, u64), b: (u64, u64)) -> std::cmp::Ordering {
    (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
}

/// One placement round of the load rule over `hosts`, a request at a time in queue order.
#[derive(Debug)]
pub struct Placer {
    hosts: Vec<Host>,
    term: u64,
    next_seq: u64,
    /// Workers held for a blocked head, by index: no later request is placed there.
    reserved: BTreeSet<usize>,
    /// The pools that have a head holding a worker this round.
    held_pools: BTreeSet<usize>,
    /// The worker each head held last round.
    kept: BTreeMap<OperationId, WorkerId>,
    /// The worker each head holds this round.
    holds: BTreeMap<OperationId, WorkerId>,
}

impl Placer {
    /// A round over `hosts`, which must be in name order, handing out lease numbers from
    /// `next_seq` of `term`; `kept` is what [`Placer::holds`] gave the round before.
    pub fn new(
        mut hosts: Vec<Host>,
        term: u64,
        next_seq: u64,
        kept: BTreeMap<OperationId, WorkerId>,
    ) -> Self {
        assert!(
            hosts.windows(2).all(|w| w[0].name < w[1].name),
            "hosts are in name order"
        );
        hosts.shrink_to_fit();
        Self {
            hosts,
            term,
            next_seq,
            reserved: BTreeSet::new(),
            held_pools: BTreeSet::new(),
            kept,
            holds: BTreeMap::new(),
        }
    }

    /// The workers the heads hold now.
    pub fn holds(&self) -> &BTreeMap<OperationId, WorkerId> {
        &self.holds
    }

    /// The candidates of `ask` that no head holds and that have room for it but for the
    /// load.
    fn roomy(&self, ask: &Ask<'_>) -> Vec<usize> {
        ask.candidates
            .iter()
            .copied()
            .filter(|i| !self.reserved.contains(i) && self.hosts[*i].has_room(&ask.resources))
            .collect()
    }

    /// Whether some worker takes `ask` now, without placing it.
    pub fn fits(&self, ask: &Ask<'_>) -> bool {
        self.roomy(ask)
            .iter()
            .any(|i| self.hosts[*i].within_limits(&ask.resources))
    }

    /// Whether some worker that has room for `ask` is charged past its limit already: the
    /// load keeps the request off it though nothing else does.
    pub fn kept_off_overcharged(&self, ask: &Ask<'_>) -> bool {
        self.roomy(ask).iter().any(|i| {
            let h = &self.hosts[*i];
            h.left() > h.cpu_limit() && !h.within_limits(&ask.resources)
        })
    }

    /// Places `ask`: the worker the load rule chooses, booked, or `None` if none admits it;
    /// and a request that has room somewhere but for the load, the first of its pool, holds
    /// the worker with the smallest binding ratio.
    pub fn place(&mut self, ask: &Ask<'_>) -> Option<usize> {
        let roomy = self.roomy(ask);
        let admitted: Vec<usize> = roomy
            .iter()
            .copied()
            .filter(|i| self.hosts[*i].within_limits(&ask.resources))
            .collect();
        let lease_seq = self.next_seq;
        let Some(chosen) = self.choose(ask, &admitted, lease_seq) else {
            if !roomy.is_empty() && !self.held_pools.contains(&ask.pool) {
                self.hold(ask);
            }
            return None;
        };
        self.hosts[chosen].book(ask.key, &ask.resources);
        self.next_seq += 1;
        Some(chosen)
    }

    /// The worker among `admitted` the request goes to. A worker the request would own
    /// whole is not scored: the score ranks those that stay shared, one of them is taken
    /// as it is, and when every worker would be owned whole the first by name is taken
    /// (each is empty).
    fn choose(&self, ask: &Ask<'_>, admitted: &[usize], seq: u64) -> Option<usize> {
        let shared: Vec<usize> = admitted
            .iter()
            .copied()
            .filter(|i| !self.hosts[*i].owns_whole(&ask.resources))
            .collect();
        match shared.as_slice() {
            [] => admitted
                .iter()
                .copied()
                .min_by_key(|i| &self.hosts[*i].name),
            [only] => Some(*only),
            _ => shared
                .into_iter()
                .min_by_key(|i| self.rank(&self.hosts[*i], ask, seq)),
        }
    }

    fn rank(&self, host: &Host, ask: &Ask<'_>, seq: u64) -> Rank {
        let (left, new) = (host.left(), host.new_charge(&ask.resources));
        let (leftover, over) = residual(host.capacity.cpu_millis, left, new);
        // Memory: nothing is charged, so the leftover is all of it.
        let (memory_leftover, _) = residual(host.capacity.memory_bytes, 0, 0);
        let (lo, hi) = (leftover.min(memory_leftover), leftover.max(memory_leftover));
        let same_key: u64 = host
            .leases
            .iter()
            .filter(|(key, _)| key == ask.key)
            .map(|(_, cpu)| *cpu)
            .sum();
        let p_bar = host.charge_of(&ask.resources);
        Rank {
            slack: SLOT_WEIGHTS * over.saturating_sub(TOLERANCE_PPM),
            memory_unsafe: false,
            concentrated: u128::from(same_key + p_bar) * PPM
                > KEY_SHARE_PPM * u128::from(host.capacity.cpu_millis),
            fit: FIT_WEIGHTS * (2 * lo * lo + BALANCE * (hi - lo) * (hi - lo)),
            uncovered: host.leases.len(),
            larger: Reverse(host.capacity.cpu_millis),
            headroom: Reverse(i128::from(host.capacity.cpu_millis) - i128::from(left)),
            hash: tie_hash(self.term, seq, &host.name),
            name: host.name.clone(),
        }
    }

    /// A blocked head holds, of the workers it could run on but for the load that no
    /// head holds and that it would not own whole, the one it would leave the least full
    /// (its binding ratio, the cores charged with its own over the worker's cores), keeping
    /// the worker it held last round while that is within a tenth of the best.
    fn hold(&mut self, ask: &Ask<'_>) {
        let candidates: Vec<(usize, (u64, u64))> = self
            .roomy(ask)
            .into_iter()
            .filter(|i| !self.hosts[*i].owns_whole(&ask.resources))
            .map(|i| {
                let h = &self.hosts[i];
                let ratio = (
                    h.left() + h.new_charge(&ask.resources),
                    h.capacity.cpu_millis,
                );
                (i, ratio)
            })
            .collect();
        // The ratio of a worker with no cores is 0, as is the memory side's.
        let ratio = |r: (u64, u64)| if r.1 == 0 { (0, 1) } else { r };
        let Some(best) = candidates
            .iter()
            .min_by(|a, b| share_cmp(ratio(a.1), ratio(b.1)))
        else {
            return;
        };
        let kept = self.kept.get(&ask.operation).and_then(|name| {
            candidates
                .iter()
                .find(|(i, _)| self.hosts[*i].name == *name)
        });
        // `r` is within a tenth of `b` when `r.0 / r.1 <= b.0 / b.1 + 1 / 10`.
        let within_a_tenth = |r: (u64, u64), b: (u64, u64)| {
            u128::from(r.0) * u128::from(b.1) * 10
                <= (u128::from(b.0) * 10 + u128::from(b.1)) * u128::from(r.1)
        };
        let pick = kept
            .filter(|k| within_a_tenth(ratio(k.1), ratio(best.1)))
            .unwrap_or(best)
            .0;
        self.reserved.insert(pick);
        self.held_pools.insert(ask.pool);
        self.holds
            .insert(ask.operation, self.hosts[pick].name.clone());
    }
}
