//! Where a lease goes: the admission predicate says which workers may take it, and this
//! module says which of them does (`docs/design/placement-by-load.md`, Scoring).
//!
//! The score ranks only the workers the predicate passed. It is handed no worker the
//! predicate refused, so it cannot admit anything and cannot move the fault bound. It
//! compares the new lease's charge with each candidate's predicted load, slot by slot, and
//! prefers the worker the lease fits most tightly. It is integer arithmetic only, so a
//! replay ranks the same way every time. Time is in logical minutes; nothing here reads a
//! clock.
//!
//! Everything the score needs about costs and load comes through one seam,
//! [`PredictedLoad`]. The flat rule of the predicate charges a lease the same at every
//! slot, so [`FlatLoad`] answers every slot with the same figures; a rule that charges by
//! a curve answers by slot and the scorer does not change.
//!
//! The previous ranking ([`Load`], [`least_loaded`]) is kept as a pure function for the
//! shadow log and for leases that bypass scoring.

use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;

use js_types::sched::usage::LeaseCost;
use js_types::sched::{LeaseId, Resources, WorkKey, WorkerId};

/// A worker's load; the smaller is the less loaded. This is the previous ranking: placement
/// by load chooses by the score ([`Scorer`]) and keeps this only for the shadow log
/// ([`least_loaded`]) and for a lease that bypasses scoring.
///
/// The load is the largest of its CPU share, its
/// memory share and its lease share (its leases of its lease cap, if it reports one) of
/// what is booked, then how many leases it holds. Under admission by load the figures are
/// what the load predicate charges the worker before a new lease (measured plus predicted
/// cost, [`crate::admission`]) instead of what is booked, as exact fractions all the same.
///
/// Both shares count because the node's limits differ in what happens at the end: a node
/// out of cores runs its actions slowly, a node out of memory kills them. A share is not
/// capped at 1: admission books no memory ([`crate::scheduler::Worker::has_room`]), so a
/// worker can hold leases whose memory is more than it has, and the worker with more
/// of it booked is still the one to avoid. (A cap would make every such worker a tie,
/// exactly when the farm is busy.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Load {
    share: Share,
    leases: usize,
}

impl Load {
    /// The load of a worker of `capacity` that has `booked` booked in `leases` leases.
    pub(crate) fn new(booked: Resources, capacity: Resources, leases: usize) -> Self {
        let cpu = Share::new(booked.cpu_millis, capacity.cpu_millis);
        let memory = Share::new(booked.memory_bytes, capacity.memory_bytes);
        // A node with no lease cap (or a cap of 0) has no lease share to be short of.
        let held = u64::try_from(leases).unwrap_or(u64::MAX);
        let lease_cap = Share::new(held, capacity.leases.unwrap_or(0));
        Self {
            share: cpu.max(memory).max(lease_cap),
            leases,
        }
    }
}

/// `booked / capacity`, compared exactly: placement is replayed by the simulator and
/// must give the same choice every time, which a float need not.
#[derive(Clone, Copy, Debug)]
struct Share {
    booked: u64,
    capacity: u64,
}

impl Share {
    /// A resource the worker has none of is one it cannot be short of: its share is 0
    /// whatever is booked, and nothing divides by zero.
    fn new(booked: u64, capacity: u64) -> Self {
        if capacity == 0 {
            Self {
                booked: 0,
                capacity: 1,
            }
        } else {
            Self { booked, capacity }
        }
    }
}

impl Ord for Share {
    fn cmp(&self, other: &Self) -> Ordering {
        // `a / b` against `c / d`, with `b` and `d` above 0, is `a * d` against `c * b`;
        // a product of two `u64`s fits a `u128`.
        (u128::from(self.booked) * u128::from(other.capacity))
            .cmp(&(u128::from(other.booked) * u128::from(self.capacity)))
    }
}

impl PartialOrd for Share {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Share {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Share {}

// ---- scoring ----

/// The horizon `H` of the predicate, in minutes.
pub const HORIZON: usize = 30;

/// The slots the predicate charges: minutes `0..=HORIZON`, then the `H+` slot.
pub const SLOTS: usize = HORIZON + 2;

/// The index of the `H+` slot: every lease charged its remaining maximum from `HORIZON` on.
pub const H_PLUS: usize = HORIZON + 1;

/// The slots at the start that the third tie-break looks at: the next five minutes.
const NEAR_SLOTS: usize = 5;

/// Evaluations one scored worker costs: every slot, for CPU and for memory.
pub const EVALUATIONS_PER_WORKER: u64 = 2 * SLOTS as u64;

/// A million parts: residuals are in parts per million of the worker's own capacity.
const PPM: u128 = 1_000_000;

/// Memory miss factor for a warm key, 13/10 (the memory target `k_m`).
const WARM_MEMORY_MISS: (u128, u128) = (13, 10);

/// Memory miss factor for a cold key, 163/100.
const COLD_MEMORY_MISS: (u128, u128) = (163, 100);

/// The largest `--placement-score-balance`.
pub const MAX_BALANCE: u32 = 4;

/// A charge or a load at one slot: CPU in millicores, memory in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charge {
    /// Millicores.
    pub cpu_millis: u64,
    /// Bytes.
    pub memory_bytes: u64,
}

/// What the score needs to know about the lease being placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseFacts {
    /// The lease, hashed into the last tie-break.
    pub lease: LeaseId,
    /// Whether its action key is cold (no learned cost). A cold key's memory is held to
    /// the larger miss factor.
    pub cold: bool,
}

/// What the score needs to know about one candidate worker, besides its curves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// `C_cpu`: the worker's CPU capacity in millicores (100%, not the cap).
    pub cpu_capacity_millis: u64,
    /// `C_mem`: the worker's memory capacity in bytes.
    pub memory_capacity_bytes: u64,
    /// `L`: the leading minutes at least half of the key's runs reached (`life_minutes`
    /// of `js-sizing`). 1 when the lease is charged flat on this worker. Clamped to
    /// `1..=HORIZON` here.
    pub fit_minutes: u8,
    /// The worker is in fallback: no measurement, or a stale signal, or a kind of node
    /// that reports none.
    pub fallback: bool,
    /// The measured covered CPU exceeds the predicted sum of the worker's covered leases
    /// (`M > P_c`).
    pub measured_above_predicted: bool,
    /// The worker's leases (covered and uncovered) of the new lease's action key, summed
    /// as the key's flat cost `p_bar`, in millicores.
    pub same_key_millis: u64,
    /// The new lease's own `p_bar`, in millicores.
    pub new_key_millis: u64,
    /// How many of the worker's leases are uncovered by its last measurement.
    pub uncovered_leases: u32,
}

/// The one seam the score reads costs and load through. The admission predicate provides
/// it from the cost records of the worker's leases; tests provide a fake.
///
/// A slot is `0..=HORIZON` for that many minutes from now, or [`H_PLUS`].
pub trait PredictedLoad {
    /// The lease being placed.
    fn lease(&self) -> LeaseFacts;

    /// The predicate's left side at `slot` for `worker` without the new lease: its
    /// predicted CPU `Lc` and memory `Lm` load.
    fn load(&self, worker: &WorkerId, slot: usize) -> Charge;

    /// The new lease's charge at `slot` on `worker`, exactly as the predicate charges
    /// it: its curve, or the flat charge at every slot for a lease charged flat there.
    fn charge(&self, worker: &WorkerId, slot: usize) -> Charge;

    /// The scalars of `worker`.
    fn host(&self, worker: &WorkerId) -> HostFacts;
}

/// The flags of the score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoreConfig {
    balance: u32,
    slack_tol_ppm: u64,
    key_share_ppm: u64,
}

impl ScoreConfig {
    /// `--placement-score-balance` `balance` (0 to [`MAX_BALANCE`]),
    /// `--placement-score-slack-tol-ppm` and `--placement-key-share` in parts per million.
    /// `None` when `balance` is above the range.
    #[must_use]
    pub const fn new(balance: u32, slack_tol_ppm: u64, key_share_ppm: u64) -> Option<Self> {
        if balance > MAX_BALANCE {
            return None;
        }
        Some(Self {
            balance,
            slack_tol_ppm,
            key_share_ppm,
        })
    }
}

impl ScoreConfig {
    /// The score flags of `cfg`, each brought into its range.
    #[must_use]
    pub fn of(cfg: &crate::LoadConfig) -> Self {
        Self {
            balance: cfg.score_balance.min(MAX_BALANCE),
            slack_tol_ppm: cfg.score_slack_tol_ppm,
            key_share_ppm: u64::from(cfg.key_share_percent.min(100)) * 10_000,
        }
    }
}

impl Default for ScoreConfig {
    /// Balance 1, tolerance 1% of capacity, key share 25%.
    fn default() -> Self {
        Self {
            balance: 1,
            slack_tol_ppm: 10_000,
            key_share_ppm: 250_000,
        }
    }
}

/// The ordering of one worker for one lease; lower is better, compared field by field.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScoreKey {
    slack: u128,
    memory_unsafe: bool,
    distrusted: bool,
    concentrated: bool,
    fit: u128,
    uncovered: u32,
    larger_cpu: Reverse<u64>,
    larger_near_headroom: Reverse<i128>,
    hash: u64,
    worker: WorkerId,
}

impl ScoreKey {
    /// Whether keys 1 to 5 are equal: only the tie-breaks tell the two apart.
    fn same_primary_keys(&self, other: &Self) -> bool {
        (
            self.slack,
            self.memory_unsafe,
            self.distrusted,
            self.concentrated,
            self.fit,
        ) == (
            other.slack,
            other.memory_unsafe,
            other.distrusted,
            other.concentrated,
            other.fit,
        )
    }
}

/// How [`Scorer::choose_among`] came to its worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice<'a> {
    /// The worker chosen.
    pub worker: &'a WorkerId,
    /// Whether it was scored: false for the only candidate.
    pub scored: bool,
    /// Key 1: the chosen worker is predicted above 100% of its capacity (plus the
    /// tolerance) at some slot: no candidate fitted inside it.
    pub slack_used: bool,
    /// Key 3: the chosen worker is distrusted.
    pub distrusted: bool,
    /// Key 4: the chosen worker is concentrated.
    pub concentrated: bool,
    /// The best two candidates were equal in keys 1 to 5 and a tie-break decided.
    pub tie_break: bool,
}

/// Ranks the workers the predicate passed.
#[derive(Clone, Debug, Default)]
pub struct Scorer {
    config: ScoreConfig,
    evaluations: u64,
}

impl Scorer {
    /// A scorer with `config`.
    #[must_use]
    pub const fn new(config: ScoreConfig) -> Self {
        Self {
            config,
            evaluations: 0,
        }
    }

    /// How many slot-resource evaluations this scorer has made.
    #[must_use]
    pub const fn evaluations(&self) -> u64 {
        self.evaluations
    }

    /// The worker among `candidates` the lease goes to; `None` for no candidate. One
    /// candidate is taken and nothing is scored. `candidates` are workers that passed
    /// the predicate; the score never adds one.
    pub fn choose<'a, M: PredictedLoad>(
        &mut self,
        model: &M,
        candidates: &'a [WorkerId],
    ) -> Option<&'a WorkerId> {
        let refs: Vec<&'a WorkerId> = candidates.iter().collect();
        self.choose_among(model, &refs).map(|c| c.worker)
    }

    /// As [`Scorer::choose`] over references, with how the choice was made.
    pub fn choose_among<'a, M: PredictedLoad>(
        &mut self,
        model: &M,
        candidates: &[&'a WorkerId],
    ) -> Option<Choice<'a>> {
        if let [only] = candidates {
            return Some(Choice {
                worker: only,
                scored: false,
                slack_used: false,
                distrusted: false,
                concentrated: false,
                tie_break: false,
            });
        }
        let mut best: Option<(ScoreKey, &'a WorkerId)> = None;
        let mut runner_up: Option<ScoreKey> = None;
        for &worker in candidates {
            let key = self.key(model, worker);
            match best.take() {
                Some((b, w)) if key >= b => {
                    if runner_up.as_ref().is_none_or(|r| key < *r) {
                        runner_up = Some(key);
                    }
                    best = Some((b, w));
                }
                previous => {
                    runner_up = previous.map(|(b, _)| b);
                    best = Some((key, worker));
                }
            }
        }
        let (key, worker) = best?;
        Some(Choice {
            worker,
            scored: true,
            slack_used: key.slack > 0,
            distrusted: key.distrusted,
            concentrated: key.concentrated,
            tie_break: runner_up.is_some_and(|r| r.same_primary_keys(&key)),
        })
    }

    /// The key of `worker` for the lease of `model`.
    pub fn key<M: PredictedLoad>(&mut self, model: &M, worker: &WorkerId) -> ScoreKey {
        let lease = model.lease();
        let host = model.host(worker);
        let tc = host.cpu_capacity_millis;
        let tm = host.memory_capacity_bytes;
        let fit_slots = usize::from(host.fit_minutes).clamp(1, HORIZON);
        let (num, den) = if lease.cold {
            COLD_MEMORY_MISS
        } else {
            WARM_MEMORY_MISS
        };
        let tol = u128::from(self.config.slack_tol_ppm);
        let balance = u128::from(self.config.balance);

        let mut slack = 0u128;
        let mut memory_unsafe = false;
        let mut fit = 0u128;
        let mut near = i128::MAX;
        for slot in 0..SLOTS {
            let load = model.load(worker, slot);
            let new = model.charge(worker, slot);
            let (xc, oc) = residual(tc, load.cpu_millis, new.cpu_millis);
            let (xm, om) = residual(tm, load.memory_bytes, new.memory_bytes);
            let w = weight(slot);
            slack += w * (oc.saturating_sub(tol) + om.saturating_sub(tol));
            let padded =
                u128::from(load.memory_bytes) + (u128::from(new.memory_bytes) * num).div_ceil(den);
            memory_unsafe |= padded > u128::from(tm);
            if slot < fit_slots {
                let (lo, hi) = (xc.min(xm), xc.max(xm));
                fit += w * (2 * lo * lo + balance * (hi - lo) * (hi - lo));
            }
            if slot < NEAR_SLOTS {
                near = near.min(i128::from(tc) - i128::from(load.cpu_millis));
            }
        }
        self.evaluations += EVALUATIONS_PER_WORKER;

        let same_key = u128::from(host.same_key_millis) + u128::from(host.new_key_millis);
        ScoreKey {
            slack,
            memory_unsafe,
            distrusted: host.fallback || host.measured_above_predicted,
            concentrated: same_key * PPM > u128::from(self.config.key_share_ppm) * u128::from(tc),
            fit,
            uncovered: host.uncovered_leases,
            larger_cpu: Reverse(tc),
            larger_near_headroom: Reverse(near),
            hash: tie_hash(lease.lease, worker),
            worker: worker.clone(),
        }
    }
}

/// The weight of `slot`: 31 for minute 0 down to 1 for minute 30, and 1 for `H+`.
fn weight(slot: usize) -> u128 {
    if slot == H_PLUS {
        1
    } else {
        (HORIZON + 1 - slot) as u128
    }
}

/// `(x, o)` for a resource of capacity `t` carrying `load` and `new`: the leftover `x`
/// and the overshoot `o` in parts per million of `t`, rounded down. One of them is 0. A
/// capacity of 0 is taken as 1, so nothing divides by zero.
fn residual(t: u64, load: u64, new: u64) -> (u128, u128) {
    let t = t.max(1);
    let d = i128::from(t) - i128::from(load) - i128::from(new);
    let scaled = d.unsigned_abs() * PPM / u128::from(t);
    if d >= 0 { (scaled, 0) } else { (0, scaled) }
}

/// The 64-bit FNV-1a hash of `len(lease) as u32 LE, lease, len(worker) as u32 LE,
/// worker`, where `lease` is the lease's `term-seq` text.
fn tie_hash(lease: LeaseId, worker: &WorkerId) -> u64 {
    let lease = format!("{}-{}", lease.term, lease.seq);
    fnv1a(&[lease.as_bytes(), worker.as_str().as_bytes()])
}

/// The digest a lease's cost record keeps of its action key: the 64-bit FNV-1a hash of
/// `len(class) as u32 LE, class, len(dedup) as u32 LE, dedup`. Never 0: 0 in a cost record
/// means "no key known" and matches nothing.
#[must_use]
pub fn key_digest(key: &WorkKey) -> u64 {
    fnv1a(&[key.class.as_bytes(), key.dedup.as_bytes()]).max(1)
}

/// FNV-1a over each part preceded by its length as a `u32` in little-endian bytes, so that
/// `("ab", "c")` and `("a", "bc")` differ. Not a seeded std hasher: the figure is the same
/// in every process.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
        for byte in len.to_le_bytes().iter().chain(*part) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// The least loaded of `admitted` by the previous ranking, the first of equals. Kept for
/// the shadow log and for a lease that bypasses scoring.
pub(crate) fn least_loaded<'a>(admitted: &[(&'a WorkerId, Load)]) -> Option<&'a WorkerId> {
    admitted
        .iter()
        .min_by_key(|(_, load)| *load)
        .map(|(name, _)| *name)
}

/// The figures of one candidate in a [`FlatLoad`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct FlatHost {
    /// The predicate's left side, without the new lease.
    pub(crate) left: Charge,
    /// The new lease's charge on this worker, as the predicate charges it (clamped).
    pub(crate) new: Charge,
    pub(crate) facts: HostFacts,
}

/// The predicted load of the candidates under the flat rule: the predicate charges every
/// lease the same at every slot, so the left side and the new lease's charge are constants
/// and `L` is 1.
#[derive(Clone, Debug)]
pub(crate) struct FlatLoad {
    pub(crate) lease: LeaseFacts,
    pub(crate) hosts: BTreeMap<WorkerId, FlatHost>,
}

impl FlatLoad {
    /// The figures of a candidate: what the predicate computed for it. `facts` has the
    /// scalars the ledger gave; the capacity, `L` and the new lease's `p_bar` are set here.
    pub(crate) fn host(
        capacity: &Resources,
        left: Charge,
        cost: &LeaseCost,
        facts: HostFacts,
    ) -> FlatHost {
        FlatHost {
            left,
            new: Charge {
                cpu_millis: cost.cpu_millis,
                memory_bytes: cost.memory_bytes,
            },
            facts: HostFacts {
                cpu_capacity_millis: capacity.cpu_millis,
                memory_capacity_bytes: capacity.memory_bytes,
                fit_minutes: 1,
                new_key_millis: cost.cpu_millis,
                ..facts
            },
        }
    }
}

impl PredictedLoad for FlatLoad {
    fn lease(&self) -> LeaseFacts {
        self.lease
    }

    fn load(&self, worker: &WorkerId, _slot: usize) -> Charge {
        self.hosts[worker].left
    }

    fn charge(&self, worker: &WorkerId, _slot: usize) -> Charge {
        self.hosts[worker].new
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        self.hosts[worker].facts
    }
}

#[cfg(test)]
mod shares;
#[cfg(test)]
mod tests;
