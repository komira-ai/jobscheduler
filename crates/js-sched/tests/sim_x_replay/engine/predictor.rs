//! What the scheduler believes about a work key: the learner of sim x.
//!
//! A key has a ring of its last [`RING`] runs that ended `Ok`. A lease sees the ring as it
//! stood when the lease arrived; with at least [`MIN_RING`] runs in it the key is warm and
//! the belief is a curve, else it is cold and the belief is flat.
//!
//! # The clock
//!
//! The ring is filled on the trace clock: a run enters its key's ring at
//! `start_ms + wall_ms` of its fixture row, and a lease that starts at `start_ms` sees the
//! runs whose end is strictly before that. Nothing the engine does (a wait in the queue, a
//! refusal, the order in which it places leases, the ranker) is an input, so the belief of
//! every lease is a function of the fixture and of the series the runs are observed to have
//! had, and is the same for every ranker. [`Predictor::build`] takes exactly those two.
//!
//! # What the learner sees
//!
//! A finished run shows the learner one [`Observed`]: its mean cores and its working set at
//! each minute it touched. Where the series come from (the truth model, or a flat stand-in
//! for a hand trace) is the caller's business; this module imports only the fixture types,
//! so it cannot read what really happens to a lease, only what the lease left behind.
//!
//! # The belief
//!
//! * Cold (fewer than [`MIN_RING`] runs): [`COLD_CORES_MILLI`] and [`COLD_MEM_MIB`] at every
//!   minute, `L = 1`.
//! * Warm: the cores curve at minute `t` is the nearest-rank P90 (see
//!   [`nearest_rank_p90`]) of the minute-`t` cores of the ring runs that reached minute `t`;
//!   the memory curve is the same for the working set. Past the last point of the curve the
//!   last point is held ([`Tail::Held`], the shipped semantics); [`Tail::Eol`] drops a warm
//!   belief to zero from minute `L` on and is for reports only.
//! * `p_bar`, the flat charge of a lease: the P90 of the ring runs' mean cores and the P90
//!   of their peak working set. It is a different quantity from the `flat_level` of the
//!   scorer's flat fit (the area of the curve over `L`), which is not computed here.
//! * `L`: [`life_minutes_standin`].
//!
//! All values are integers: cores in millicores, memory in MiB.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use super::fixture::{KeyIx, Outcome, Row, fnv1a};

/// Runs a key's ring keeps.
pub const RING: usize = 32;

/// Runs a ring needs before its key is warm.
pub const MIN_RING: usize = 3;

/// The flat charge of a cold lease, millicores (2 cores).
pub const COLD_CORES_MILLI: u64 = 2_000;

/// The flat charge of a cold lease, MiB (4 GiB).
pub const COLD_MEM_MIB: u64 = 4_096;

/// The most minutes of a lease's life the fit looks at.
pub const LIFE_HORIZON: u8 = 30;

const MINUTE_MS: u64 = 60_000;

/// `L`, the life of a key in whole minutes, from the last minute each ring run reached.
///
/// A stand-in for the life count of the sizing crate's summary (`Summary::life_minutes`),
/// to be deleted when that count is reachable from the engine, with its tests moved to it.
/// `reached[i]` is the last minute (0-indexed) run `i` touched: its wall time in whole
/// minutes, saturating at 255; a run under a minute reached minute 0. The result is the
/// number of leading minutes `t` that at least half of the runs reached, clamped to
/// `1..=`[`LIFE_HORIZON`]. No runs, or runs all under a minute, give 1.
pub fn life_minutes_standin(reached: &[u8]) -> u8 {
    let kept = reached.len();
    let at_least_half = |t: u8| reached.iter().filter(|m| **m >= t).count() * 2 >= kept;
    let life = (0..LIFE_HORIZON)
        .take_while(|t| kept > 0 && at_least_half(*t))
        .count();
    u8::try_from(life)
        .unwrap_or(LIFE_HORIZON)
        .clamp(1, LIFE_HORIZON)
}

/// The nearest-rank P90 of `values`: the `ceil(0.9 n)`-th smallest. The slice is sorted
/// in place. An empty slice gives 0.
pub fn nearest_rank_p90(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    let rank = (values.len() * 9).div_ceil(10);
    rank.checked_sub(1)
        .and_then(|i| values.get(i))
        .copied()
        .unwrap_or(0)
}

/// A run's mean cores, in millicores: `cpu_ms / wall_ms` rounded down.
pub fn mean_cores_milli(row: &Row) -> u64 {
    let milli = u128::from(row.cpu_ms) * 1_000 / u128::from(row.wall_ms.max(1));
    u64::try_from(milli).unwrap_or(u64::MAX)
}

/// The last minute (0-indexed) a run reached: whole minutes of wall time, saturating.
pub fn minutes_reached(row: &Row) -> u8 {
    u8::try_from(row.wall_ms / MINUTE_MS).unwrap_or(u8::MAX)
}

/// What the learner saw of one finished run: its mean cores and working set at each minute
/// it touched. Never empty; the two series have one length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    cores_milli: Vec<u64>,
    ws_mib: Vec<u64>,
}

impl Observed {
    /// A series, or `None` if it is empty or the two lengths differ.
    pub fn new(cores_milli: Vec<u64>, ws_mib: Vec<u64>) -> Option<Observed> {
        (!cores_milli.is_empty() && cores_milli.len() == ws_mib.len()).then_some(Observed {
            cores_milli,
            ws_mib,
        })
    }

    /// The series of a run with no shape: its mean cores and its peak memory at each of
    /// the `wall_ms / 60,000 + 1` minutes it reached (one point for a run under a minute).
    pub fn flat(row: &Row) -> Observed {
        let points = usize::from(minutes_reached(row)) + 1;
        Observed {
            cores_milli: vec![mean_cores_milli(row); points],
            ws_mib: vec![row.peak_mem_mib; points],
        }
    }

    /// Minutes the run touched.
    pub fn minutes(&self) -> usize {
        self.cores_milli.len()
    }

    fn peak_mib(&self) -> u64 {
        self.ws_mib.iter().copied().max().unwrap_or(0)
    }
}

/// What a belief does past the last point of its curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    /// The last point is held (what the shipped curve does).
    Held,
    /// A warm belief is zero from minute `L` on. Report-only.
    Eol,
}

/// The per-minute values of a warm belief, or the single point of a cold one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Curves {
    cores_milli: Vec<u64>,
    mem_mib: Vec<u64>,
}

/// The belief of one lease. Cloning shares the curves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Belief {
    cold: bool,
    life: u8,
    p_bar_cores_milli: u64,
    p_bar_mem_mib: u64,
    curves: Arc<Curves>,
}

impl Belief {
    fn cold(curves: &Arc<Curves>) -> Belief {
        Belief {
            cold: true,
            life: 1,
            p_bar_cores_milli: COLD_CORES_MILLI,
            p_bar_mem_mib: COLD_MEM_MIB,
            curves: Arc::clone(curves),
        }
    }

    /// Whether the key had fewer than [`MIN_RING`] runs in its ring.
    pub fn is_cold(&self) -> bool {
        self.cold
    }

    /// `L`, at least 1.
    pub fn life_minutes(&self) -> u8 {
        self.life
    }

    /// `p_bar`, cores: P90 of the ring runs' mean cores; cold, 2 cores.
    pub fn p_bar_cores_milli(&self) -> u64 {
        self.p_bar_cores_milli
    }

    /// `p_bar`, memory: P90 of the ring runs' peak working set; cold, 4 GiB.
    pub fn p_bar_mem_mib(&self) -> u64 {
        self.p_bar_mem_mib
    }

    /// Points in the curve (1 when cold).
    pub fn curve_len(&self) -> usize {
        self.curves.cores_milli.len()
    }

    /// Whether two beliefs read the same cached curve, not merely equal ones.
    pub fn shares_curves_with(&self, other: &Belief) -> bool {
        Arc::ptr_eq(&self.curves, &other.curves)
    }

    /// Predicted cores at `minute` of the lease's life, millicores.
    pub fn cores_milli_at(&self, minute: usize, tail: Tail) -> u64 {
        self.at(&self.curves.cores_milli, minute, tail)
    }

    /// Predicted working set at `minute` of the lease's life, MiB.
    pub fn mem_mib_at(&self, minute: usize, tail: Tail) -> u64 {
        self.at(&self.curves.mem_mib, minute, tail)
    }

    fn at(&self, points: &[u64], minute: usize, tail: Tail) -> u64 {
        if tail == Tail::Eol && !self.cold && minute >= usize::from(self.life) {
            return 0;
        }
        points
            .get(minute.min(points.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    }

    /// Appends a canonical form of the belief: flag, `L`, `p_bar`, then both curves.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(u8::from(self.cold));
        out.push(self.life);
        out.extend_from_slice(&self.p_bar_cores_milli.to_le_bytes());
        out.extend_from_slice(&self.p_bar_mem_mib.to_le_bytes());
        let len = u32::try_from(self.curve_len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        for v in self.curves.cores_milli.iter().chain(&self.curves.mem_mib) {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// One finished run in a ring.
struct Entry {
    reached: u8,
    mean_cores_milli: u64,
    series: Observed,
}

/// The belief for a ring: cold below [`MIN_RING`] runs, else the P90 curves.
fn believe(ring: &VecDeque<Entry>, cold: &Arc<Curves>) -> Belief {
    if ring.len() < MIN_RING {
        return Belief::cold(cold);
    }
    let reached: Vec<u8> = ring.iter().map(|e| e.reached).collect();
    let mut means: Vec<u64> = ring.iter().map(|e| e.mean_cores_milli).collect();
    let mut peaks: Vec<u64> = ring.iter().map(|e| e.series.peak_mib()).collect();
    let minutes = ring.iter().map(|e| e.series.minutes()).max().unwrap_or(1);
    let point = |pick: fn(&Observed) -> &[u64], t: usize| {
        let mut values: Vec<u64> = ring
            .iter()
            .filter_map(|e| pick(&e.series).get(t).copied())
            .collect();
        nearest_rank_p90(&mut values)
    };
    let cores_milli = (0..minutes).map(|t| point(|s| &s.cores_milli, t)).collect();
    let mem_mib = (0..minutes).map(|t| point(|s| &s.ws_mib, t)).collect();
    Belief {
        cold: false,
        life: life_minutes_standin(&reached),
        p_bar_cores_milli: nearest_rank_p90(&mut means),
        p_bar_mem_mib: nearest_rank_p90(&mut peaks),
        curves: Arc::new(Curves {
            cores_milli,
            mem_mib,
        }),
    }
}

/// The belief of every lease of a fixture, indexed by the lease's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Predictor {
    beliefs: Vec<Belief>,
}

impl Predictor {
    /// The beliefs for `rows` (row `i` is lease `i`), on the trace clock.
    ///
    /// `observed(i, row)` gives what the learner saw of run `i` once it ended; it is
    /// called once for each `Ok` run that some later lease can see, in no promised order,
    /// and must be a function of its arguments (and the caller's seed) alone.
    pub fn build(rows: &[Row], mut observed: impl FnMut(usize, &Row) -> Observed) -> Predictor {
        let mut by_key: BTreeMap<KeyIx, Vec<usize>> = BTreeMap::new();
        for (i, row) in rows.iter().enumerate() {
            by_key.entry(row.key).or_default().push(i);
        }
        let cold = Arc::new(Curves {
            cores_milli: vec![COLD_CORES_MILLI],
            mem_mib: vec![COLD_MEM_MIB],
        });
        let mut made: Vec<(usize, Belief)> = Vec::with_capacity(rows.len());
        for ixs in by_key.values() {
            let end = |i: usize| rows[i].start_ms.saturating_add(rows[i].wall_ms);
            let mut arrivals = ixs.clone();
            arrivals.sort_by_key(|&i| (rows[i].start_ms, i));
            let mut ends: Vec<usize> = ixs
                .iter()
                .copied()
                .filter(|&i| rows[i].outcome == Outcome::Ok)
                .collect();
            ends.sort_by_key(|&i| (end(i), i));
            let mut next_end = 0;
            let mut ring: VecDeque<Entry> = VecDeque::with_capacity(RING + 1);
            // The belief of the ring as it stands: dropped on every push, so a lease
            // never reads a curve built before the latest run it can see.
            let mut cached: Option<Belief> = None;
            for i in arrivals {
                while next_end < ends.len() && end(ends[next_end]) < rows[i].start_ms {
                    let j = ends[next_end];
                    next_end += 1;
                    if ring.len() == RING {
                        ring.pop_front();
                    }
                    ring.push_back(Entry {
                        reached: minutes_reached(&rows[j]),
                        mean_cores_milli: mean_cores_milli(&rows[j]),
                        series: observed(j, &rows[j]),
                    });
                    cached = None;
                }
                let belief = cached.get_or_insert_with(|| believe(&ring, &cold));
                made.push((i, belief.clone()));
            }
        }
        made.sort_by_key(|(i, _)| *i);
        Predictor {
            beliefs: made.into_iter().map(|(_, b)| b).collect(),
        }
    }

    /// The belief of lease `lease_ix`.
    ///
    /// # Panics
    /// If `lease_ix` is not a row of the fixture the predictor was built from.
    pub fn belief(&self, lease_ix: usize) -> &Belief {
        &self.beliefs[lease_ix]
    }

    /// Leases.
    pub fn len(&self) -> usize {
        self.beliefs.len()
    }

    /// Whether there are no leases.
    pub fn is_empty(&self) -> bool {
        self.beliefs.is_empty()
    }

    /// Every belief in lease order, in the canonical form of [`Belief::encode`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for b in &self.beliefs {
            b.encode(&mut out);
        }
        out
    }

    /// FNV-1a of [`Predictor::to_bytes`].
    pub fn digest(&self) -> u64 {
        fnv1a(&self.to_bytes())
    }
}
