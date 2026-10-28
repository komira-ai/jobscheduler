//! Tests of rankers 3, 4, 5 and FW5: that the three fits choose differently where they
//! should, that the fit knob touches key 5 and nothing else, the decided-key shares, the
//! decides-differently counters and candidate-order invariance.
//!
//! Each test names the defect it would catch. None reads a clock or asserts a timing; the
//! figures are integers, and a float appears nowhere.

use js_sched::{
    Charge, DecidedBy, EVALUATIONS_PER_WORKER, FitShape, PredictedLoad, SLOTS, ScoreConfig,
    ScoreKey, Scorer,
};
use js_types::sched::WorkerId;
use std::collections::BTreeMap;

use crate::engine::adapter::{
    FLOOR_PERMILLE, HostState, LeaseBelief, ScoreModel, ScoreRanker, Slot0,
};
use crate::engine::rankers::{
    GD_LONG_PPM, GD_SCORED_MIN, GD_SCORED_PPM, Gd, Ranker, RankerKind, differing, flat_belief,
    percent, ratio_ppm, sequence_hash,
};

const MIB: u64 = 1 << 20;
/// Every host in these tables has 100 cores and, in proportion, memory, so a resource's
/// leftover is the same share on both and the scorer's balance term is zero.
const CAP: u64 = 100_000;

fn id(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// `millis` of CPU and the proportional memory.
fn cm(millis: u64) -> Charge {
    Charge {
        cpu_millis: millis,
        memory_bytes: millis * MIB,
    }
}

fn lease(ix: u64, l: u8, curve: [u64; SLOTS]) -> LeaseBelief {
    LeaseBelief {
        lease_ix: ix,
        cold: false,
        fit_minutes: l,
        p_bar_cpu_millis: curve[0],
        charge: curve.map(cm),
    }
}

/// A host predicted at `loads[s]` millicores at slot `s`, measured at slot 0's figure and
/// fully covered.
fn host(loads: [u64; SLOTS]) -> HostState {
    let mut ahead = [Charge::default(); SLOTS - 1];
    for (slot, v) in ahead.iter_mut().zip(&loads[1..]) {
        *slot = cm(*v);
    }
    HostState {
        cpu_capacity_millis: CAP,
        memory_capacity_bytes: CAP * MIB,
        slot0: Slot0 {
            measured_cpu_millis: loads[0],
            covered_cpu_millis: loads[0],
            memory_all_bytes: loads[0] * MIB,
            ..Slot0::default()
        },
        ahead,
        same_key_millis: 0,
        uncovered_leases: 0,
    }
}

fn model(lease: LeaseBelief, hosts: &[(&str, HostState)]) -> ScoreModel {
    ScoreModel::new(
        lease,
        FLOOR_PERMILLE,
        hosts.iter().map(|(n, h)| (id(n), *h)),
    )
}

fn names(list: &[&str]) -> Vec<WorkerId> {
    list.iter().map(|n| id(n)).collect()
}

fn base() -> ScoreConfig {
    ScoreConfig::default()
}

fn key(fit: FitShape, m: &impl PredictedLoad, w: &str) -> ScoreKey {
    Scorer::new(base().with_fit(fit)).key(m, &id(w))
}

/// The weight of slot `s`, restated from the scoring rule: 31 for minute 0 down to 1 for
/// minute 30.
const fn weight(s: usize) -> u128 {
    31 - s as u128
}

/// `(leftover, overshoot)` of a resource in parts per million of its capacity.
fn residual(t: u64, load: u64, new: u64) -> (u128, u128) {
    let d = i128::from(t) - i128::from(load) - i128::from(new);
    let scaled = d.unsigned_abs() * 1_000_000 / u128::from(t);
    if d >= 0 { (scaled, 0) } else { (0, scaled) }
}

/// Key 5 restated from the rule for a host whose load is `load[s]` and a lease charged
/// `new[s]`, over `l` fit slots, balance 1.
fn fit_by_rule(fit: FitShape, load: &[Charge], new: &[Charge], l: usize) -> u128 {
    let two = |lc: Charge, nc: Charge| {
        let (xc, _) = residual(CAP, lc.cpu_millis, nc.cpu_millis);
        let (xm, _) = residual(CAP * MIB, lc.memory_bytes, nc.memory_bytes);
        (xc.min(xm), xc.max(xm))
    };
    match fit {
        FitShape::Squared | FitShape::Linear => (0..l)
            .map(|s| {
                let (lo, hi) = two(load[s], new[s]);
                weight(s)
                    * if fit == FitShape::Squared {
                        2 * lo * lo + (hi - lo) * (hi - lo)
                    } else {
                        2 * lo + (hi - lo)
                    }
            })
            .sum(),
        FitShape::Flat => {
            let mean = |v: &[Charge]| Charge {
                cpu_millis: (v[..l]
                    .iter()
                    .map(|c| u128::from(c.cpu_millis))
                    .sum::<u128>()
                    / l as u128) as u64,
                memory_bytes: (v[..l]
                    .iter()
                    .map(|c| u128::from(c.memory_bytes))
                    .sum::<u128>()
                    / l as u128) as u64,
            };
            let (lo, hi) = two(mean(load), mean(new));
            (0..l).map(weight).sum::<u128>() * (2 * lo * lo + (hi - lo) * (hi - lo))
        }
    }
}

// ---- the shape table ----

const L: u8 = 6;
const NEW: u64 = 7_000;

/// Leftover capacity, in thousands of millicores, at fit slots 0 to 5 of three hosts for a
/// lease charged 7 cores flat. Beyond slot 5 each host carries 30 cores, so nothing
/// overshoots and nothing is memory-unsafe: keys 1 to 4 are equal on all three.
///
/// * `a` is nearly full for the first two minutes and roomy after: the weighted sum of its
///   leftovers is the least, its squares are the largest.
/// * `b` is the middle everywhere: the least sum of squares.
/// * `c` is roomy early and nearly full late: the least mean leftover, so the least flat fit.
const SHAPES: [(&str, [u64; L as usize]); 3] = [
    ("a", [5, 5, 40, 40, 40, 40]),
    ("b", [20, 20, 35, 35, 35, 35]),
    ("c", [40, 40, 40, 15, 15, 15]),
];

fn shape_loads(leftover: &[u64; L as usize]) -> [u64; SLOTS] {
    let mut loads = [30_000; SLOTS];
    for (slot, d) in loads.iter_mut().zip(leftover) {
        *slot = CAP - NEW - d * 1_000;
    }
    loads
}

fn shape_table() -> ScoreModel {
    let hosts: Vec<(&str, HostState)> = SHAPES
        .iter()
        .map(|(n, d)| (*n, host(shape_loads(d))))
        .collect();
    model(lease(1, L, [NEW; SLOTS]), &hosts)
}

fn pick(kind: RankerKind, m: &ScoreModel, cands: &[WorkerId]) -> (String, Ranker) {
    let mut r = Ranker::new(kind, base());
    let chosen = r.place(m, cands).expect("a candidate").as_str().to_owned();
    (chosen, r)
}

/// Shows: rankers 3, 4 and 5 choose three different hosts on a table built so that the
/// weighted sum of leftovers, the sum of their squares and the square of their mean each
/// have a different winner, and each one is decided by key 5. Catches a ranker whose knob
/// is not wired (two of the three would agree), a fit that is ignored, and a decision
/// attributed to a key other than the fit.
#[test]
fn rankers_three_four_five_choose_three_hosts_on_the_shape_table() {
    let m = shape_table();
    let cands = names(&["a", "b", "c"]);
    let (linear, r3) = pick(RankerKind::Linear, &m, &cands);
    let (flat, r4) = pick(RankerKind::Flat, &m, &cands);
    let (score, r5) = pick(RankerKind::Score, &m, &cands);
    assert_eq!(
        (linear.as_str(), flat.as_str(), score.as_str()),
        ("a", "c", "b")
    );
    for r in [&r3, &r4, &r5] {
        assert_eq!(r.decisions().count(DecidedBy::Key(5)), 1);
        assert_eq!(r.decisions().total(), 1);
    }
    // Ranker 5 here is the adapter's ranker 5: `Scorer::choose` with the default flags.
    let mut adapter = ScoreRanker::new();
    assert_eq!(adapter.place(&m, &cands), Some(&id("b")));
}

/// Shows: key 5 of each ranker is what the rule says, host by host, restated from the rule
/// in this file (the weighted sum, the weighted sum of squares, the square of the mean over
/// the fit slots). Catches a Linear that squares, a Flat that averages the wrong slots or
/// weights twice, and a fit that covers more or fewer slots than `L`.
#[test]
fn key_five_is_the_rule_restated_for_each_fit() {
    let m = shape_table();
    let new = [cm(NEW); SLOTS];
    for (name, d) in SHAPES {
        let load: Vec<Charge> = shape_loads(&d).iter().map(|v| cm(*v)).collect();
        for fit in [FitShape::Squared, FitShape::Linear, FitShape::Flat] {
            assert_eq!(
                key(fit, &m, name).fit(),
                fit_by_rule(fit, &load, &new, L as usize),
                "{name} {fit:?}"
            );
        }
    }
}

fn comparable(k: &ScoreKey) -> (u128, bool, bool, bool, u32, u64, i128, u64, WorkerId) {
    (
        k.slack(),
        k.memory_unsafe(),
        k.distrusted(),
        k.concentrated(),
        k.uncovered(),
        k.cpu_capacity_millis(),
        k.near_headroom_millis(),
        k.hash(),
        k.worker().clone(),
    )
}

/// Hosts that make each of keys 1 to 4 non-zero for some host, for a lease of 10 cores and
/// 20 GiB-equivalent memory (`pressure_lease`).
fn pressure() -> (LeaseBelief, Vec<(&'static str, HostState)>) {
    let new = Charge {
        cpu_millis: 10_000,
        memory_bytes: 20_000 * MIB,
    };
    let mut l = lease(7, L, [10_000; SLOTS]);
    l.charge = [new; SLOTS];
    let flat = |cpu: u64, mem: u64| {
        let mut h = host([cpu; SLOTS]);
        h.ahead = [Charge {
            cpu_millis: cpu,
            memory_bytes: mem * MIB,
        }; SLOTS - 1];
        h.slot0.memory_all_bytes = mem * MIB;
        h
    };
    let mut distrusted = flat(40_000, 40_000);
    distrusted.slot0.measured_cpu_millis = 50_000;
    let mut concentrated = flat(40_000, 40_000);
    concentrated.same_key_millis = 30_000;
    (
        l,
        vec![
            ("ok", flat(40_000, 40_000)),
            ("slack", flat(95_000, 40_000)),
            ("memory", flat(40_000, 80_000)),
            ("distrusted", distrusted),
            ("concentrated", concentrated),
        ],
    )
}

/// Shows: the fit knob changes key 5 and nothing else. Keys 1 to 4 and every tie-break
/// figure of a host are equal under Squared, Flat and Linear, on the shape table and on a
/// table where each of keys 1 to 4 is non-zero for some host (so the equality is not the
/// equality of zeros); and Flat's key 5 differs from Squared's on the shape table, so the
/// comparison is not between two copies of one thing. Catches a Flat that averages the load
/// the slack key or the memory key reads, or the near-term headroom.
#[test]
fn keys_one_to_four_are_equal_between_squared_and_flat() {
    let m = shape_table();
    let mut differs = 0;
    for (name, _) in SHAPES {
        let sq = key(FitShape::Squared, &m, name);
        let fl = key(FitShape::Flat, &m, name);
        let li = key(FitShape::Linear, &m, name);
        assert_eq!(comparable(&sq), comparable(&fl), "{name}");
        assert_eq!(comparable(&sq), comparable(&li), "{name}");
        differs += usize::from(sq.fit() != fl.fit());
    }
    assert_eq!(differs, 3);

    let (l, hosts) = pressure();
    let m = model(l, &hosts);
    let mut seen = [false; 4];
    for (name, _) in &hosts {
        let sq = key(FitShape::Squared, &m, name);
        assert_eq!(comparable(&sq), comparable(&key(FitShape::Flat, &m, name)));
        assert_eq!(
            comparable(&sq),
            comparable(&key(FitShape::Linear, &m, name))
        );
        seen[0] |= sq.slack() > 0;
        seen[1] |= sq.memory_unsafe();
        seen[2] |= sq.distrusted();
        seen[3] |= sq.concentrated();
    }
    assert_eq!(seen, [true; 4], "each of keys 1 to 4 must fire somewhere");
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn below(state: &mut u64, n: u64) -> u64 {
    splitmix(state) % n
}

/// Shows: with every curve flat, Flat and Squared are the same ranker: whole keys equal,
/// same host, same deciding rule, over 300 seeded tables with `L` from 1 to 30, two to
/// four hosts and loads up to overshoot. Catches a Flat that is not the identity on flat
/// curves (an off-by-one in the mean, a weight applied twice). The shape table above is
/// the converse: there they differ.
#[test]
fn flat_equals_squared_when_all_curves_are_flat() {
    let mut s = 11;
    let mut scored = 0;
    for ix in 0..300 {
        let l = 1 + below(&mut s, 30) as u8;
        let hosts: Vec<(String, HostState)> = (0..2 + below(&mut s, 3))
            .map(|h| (format!("w{h}"), host([below(&mut s, 98_000); SLOTS])))
            .collect();
        let m = ScoreModel::new(
            lease(ix, l, [1_000 + below(&mut s, 19_000); SLOTS]),
            FLOOR_PERMILLE,
            hosts.iter().map(|(n, h)| (id(n), *h)),
        );
        let cands: Vec<WorkerId> = hosts.iter().map(|(n, _)| id(n)).collect();
        for (n, _) in &hosts {
            assert_eq!(
                key(FitShape::Flat, &m, n),
                key(FitShape::Squared, &m, n),
                "table {ix} host {n}"
            );
        }
        let a = Scorer::new(base().with_fit(FitShape::Flat)).choose_explained(&m, &cands);
        let b = Scorer::new(base()).choose_explained(&m, &cands);
        assert_eq!(a, b, "table {ix}");
        scored += usize::from(cands.len() >= 2);
    }
    assert_eq!(scored, 300);
}

/// A model that flattens each resident on its own: the host's load at the fit slots is the
/// sum of the residents' own means over the fit slots, the lease's charge its own mean.
/// Everything else (slot 0 aside, the scalars, the slots past the fit) is the inner model's.
struct PerResident<'a> {
    inner: &'a ScoreModel,
    l: usize,
    load: BTreeMap<WorkerId, Charge>,
    new: Charge,
}

impl PredictedLoad for PerResident<'_> {
    fn lease(&self) -> js_sched::LeaseFacts {
        self.inner.lease()
    }
    fn load(&self, worker: &WorkerId, slot: usize) -> Charge {
        if slot < self.l {
            self.load[worker]
        } else {
            self.inner.load(worker, slot)
        }
    }
    fn charge(&self, worker: &WorkerId, slot: usize) -> Charge {
        if slot < self.l {
            self.new
        } else {
            self.inner.charge(worker, slot)
        }
    }
    fn host(&self, worker: &WorkerId) -> js_sched::HostFacts {
        self.inner.host(worker)
    }
}

/// The mean over `l` slots of a resident's predicted curve, floor division.
fn mean_of(curve: &[Charge], l: usize) -> Charge {
    Charge {
        cpu_millis: (curve[..l]
            .iter()
            .map(|c| u128::from(c.cpu_millis))
            .sum::<u128>()
            / l as u128) as u64,
        memory_bytes: (curve[..l]
            .iter()
            .map(|c| u128::from(c.memory_bytes))
            .sum::<u128>()
            / l as u128) as u64,
    }
}

/// Shows: Flat is the flattening of the host aggregate, measurement included, and not of
/// each resident. First the two agree where nothing but predicted curves are in play (the
/// shape table, one resident per host), which validates the per-resident model of this
/// test; then they differ on a table where the measurement is above the prediction at slot
/// 0: both hosts carry one resident predicted at 60 cores, host `x` is measured at 70 and
/// host `y` at 60.001. The aggregate mean sees `x` as fuller and picks it; flattening the
/// resident sees two equal hosts and the tie-break (fewer uncovered leases) picks `y`.
/// Catches a Flat built from the residents' curves, which would drop the measurement from
/// the fit.
#[test]
fn flat_flattens_the_host_aggregate_not_each_resident() {
    // Agreement without a measurement term.
    let m = shape_table();
    let new = cm(NEW);
    let load: BTreeMap<WorkerId, Charge> = SHAPES
        .iter()
        .map(|(n, d)| {
            let curve: Vec<Charge> = shape_loads(d).iter().map(|v| cm(*v)).collect();
            (id(n), mean_of(&curve, L as usize))
        })
        .collect();
    let oracle = PerResident {
        inner: &m,
        l: L as usize,
        load,
        new,
    };
    for (n, _) in SHAPES {
        assert_eq!(
            Scorer::new(base()).key(&oracle, &id(n)).fit(),
            key(FitShape::Flat, &m, n).fit(),
            "{n}"
        );
    }

    // The measurement table: two fit slots.
    let resident = [cm(60_000); SLOTS];
    let mut x = host([60_000; SLOTS]);
    x.slot0.measured_cpu_millis = 70_000;
    x.uncovered_leases = 1;
    let mut y = host([60_000; SLOTS]);
    y.slot0.measured_cpu_millis = 60_001;
    let m = model(lease(2, 2, [10_000; SLOTS]), &[("x", x), ("y", y)]);
    let cands = names(&["x", "y"]);
    let oracle = PerResident {
        inner: &m,
        l: 2,
        load: [
            (id("x"), mean_of(&resident, 2)),
            (id("y"), mean_of(&resident, 2)),
        ]
        .into_iter()
        .collect(),
        new: cm(10_000),
    };
    assert!(m.measurement_wins(&id("x")) && m.measurement_wins(&id("y")));
    assert_eq!(
        Scorer::new(base().with_fit(FitShape::Flat)).choose_explained(&m, &cands),
        Some((&id("x"), DecidedBy::Key(5)))
    );
    assert_eq!(
        Scorer::new(base()).choose_explained(&oracle, &cands),
        Some((&id("y"), DecidedBy::TieBreak(1)))
    );
    // And the aggregate mean is what the rule restated says: (70,000 + 60,000) / 2.
    // (The measurement is CPU only; the memory at slot 0 is the residents' 60 cores' worth.)
    let load = [
        Charge {
            cpu_millis: 70_000,
            memory_bytes: 60_000 * MIB,
        },
        cm(60_000),
    ];
    assert_eq!(
        key(FitShape::Flat, &m, "x").fit(),
        fit_by_rule(FitShape::Flat, &load, &[cm(10_000); 2], 2)
    );
}

// ---- decided-key shares ----

/// Shows: each deciding rule is attributed to the right key: the host that overshoots
/// loses on key 1, the memory-unsafe on key 2, the distrusted on key 3, the concentrated
/// on key 4; two identical hosts are decided by the hash (tie-break 4); one candidate is
/// `Only` and is not scored. The same through all four kinds. Catches a key counted under
/// its neighbour's number and a lone candidate counted as scored.
#[test]
fn the_deciding_key_is_counted_under_its_own_number() {
    let (l, hosts) = pressure();
    let m = model(l, &hosts);
    let cases = [
        ("slack", DecidedBy::Key(1)),
        ("memory", DecidedBy::Key(2)),
        ("distrusted", DecidedBy::Key(3)),
        ("concentrated", DecidedBy::Key(4)),
    ];
    for kind in [
        RankerKind::Linear,
        RankerKind::Flat,
        RankerKind::Score,
        RankerKind::FlatScore,
    ] {
        let mut r = Ranker::new(kind, base());
        for (bad, by) in cases {
            let cands = names(&["ok", bad]);
            assert_eq!(r.place(&m, &cands), Some(&id("ok")), "{bad}");
            assert_eq!(r.decisions().count(by), 1, "{kind:?} {bad}");
        }
        let twins = model(
            lease(9, L, [NEW; SLOTS]),
            &[("t1", host([30_000; SLOTS])), ("t2", host([30_000; SLOTS]))],
        );
        assert!(r.place(&twins, &names(&["t1", "t2"])).is_some());
        assert_eq!(r.decisions().count(DecidedBy::TieBreak(4)), 1);
        assert_eq!(r.place(&twins, &names(&["t2"])), Some(&id("t2")));
        assert_eq!(r.decisions().count(DecidedBy::Only), 1);
        assert_eq!(r.attempts(), 6);
        assert_eq!(r.scored(), 5, "{kind:?}: the lone candidate is not scored");
        assert_eq!(r.decisions().total(), 6);
        assert_eq!(r.place(&twins, &[]), None);
        assert_eq!(r.attempts(), 6, "no candidate is no attempt");
    }
}

/// One placement round of the seeded stream: a lease and its hosts.
struct Round {
    lease: LeaseBelief,
    hosts: Vec<(WorkerId, HostState)>,
}

impl Round {
    fn model(&self) -> ScoreModel {
        ScoreModel::new(self.lease, FLOOR_PERMILLE, self.hosts.iter().cloned())
    }
    fn names(&self) -> Vec<WorkerId> {
        self.hosts.iter().map(|(n, _)| n.clone()).collect()
    }
}

/// A seeded stream of rounds with one to four hosts of varied shape: flat, stepping up,
/// stepping down; some measured above their prediction, some with an uncovered lease, some
/// holding the lease's key. Leases have `L` from 1 to 8 and a curve.
fn stream(n: u64, seed: u64) -> Vec<Round> {
    let mut s = seed;
    (0..n)
        .map(|ix| {
            let l = 1 + below(&mut s, 8) as u8;
            let mut curve = [0; SLOTS];
            let mut v = 2_000 + below(&mut s, 12_000);
            for (slot, c) in curve.iter_mut().enumerate() {
                if slot < usize::from(l) {
                    v = (v + below(&mut s, 6_000)).saturating_sub(2_500).max(1_000);
                }
                *c = v;
            }
            let hosts = (0..1 + below(&mut s, 4))
                .map(|h| {
                    let level = 10_000 + below(&mut s, 40_000);
                    let step = 5_000 + below(&mut s, 20_000);
                    let cut = 1 + below(&mut s, 8) as usize;
                    let shift = below(&mut s, 3);
                    let mut loads = [level; SLOTS];
                    for ld in loads.iter_mut().skip(cut) {
                        match shift {
                            1 => *ld = level + step,
                            2 => *ld = level.saturating_sub(step).max(1_000),
                            _ => {}
                        }
                    }
                    let mut st = host(loads);
                    if below(&mut s, 4) == 0 {
                        st.slot0.measured_cpu_millis += 5_000;
                    }
                    st.uncovered_leases = below(&mut s, 2) as u32;
                    if below(&mut s, 5) == 0 {
                        st.same_key_millis = 20_000;
                    }
                    (id(&format!("w{h}")), st)
                })
                .collect();
            Round {
                lease: lease(ix, l, curve),
                hosts,
            }
        })
        .collect()
}

const KINDS: [RankerKind; 4] = [
    RankerKind::Linear,
    RankerKind::Flat,
    RankerKind::Score,
    RankerKind::FlatScore,
];

fn run(kind: RankerKind, rounds: &[Round], shuffle: Option<u64>) -> Ranker {
    let mut r = Ranker::new(kind, base());
    let mut s = shuffle.unwrap_or(0);
    for round in rounds {
        let mut cands = round.names();
        if shuffle.is_some() {
            for i in (1..cands.len()).rev() {
                cands.swap(i, below(&mut s, i as u64 + 1) as usize);
            }
        }
        let m = round.model();
        let chosen = r.place(&m, &cands).expect("a host").clone();
        assert!(round.hosts.iter().any(|(n, _)| *n == chosen));
    }
    r
}

/// Shows: the decided-key shares sum to the placements, `Only` is exactly the rounds with
/// one host, scored placements are exactly the rounds with two or more, the scorer's
/// evaluation count is 64 per scored candidate for every fit (Flat included: it reads the
/// same slots), and the per-rule shares in parts per million never exceed the whole.
/// Catches a placement counted under two rules or none, and a fit that skips or repeats
/// the slot pass.
#[test]
fn shares_sum_to_placements_and_evaluations_are_64_per_scored_candidate() {
    let rounds = stream(600, 5);
    let singles = rounds.iter().filter(|r| r.hosts.len() == 1).count() as u64;
    let multi_candidates: u64 = rounds
        .iter()
        .filter(|r| r.hosts.len() >= 2)
        .map(|r| r.hosts.len() as u64)
        .sum();
    for kind in KINDS {
        let r = run(kind, &rounds, None);
        let d = r.decisions();
        assert_eq!(d.total(), 600, "{kind:?}");
        assert_eq!(r.attempts(), 600);
        assert_eq!(d.count(DecidedBy::Only), singles);
        assert_eq!(r.scored(), 600 - singles);
        assert_eq!(r.scored_candidates(), multi_candidates);
        assert_eq!(r.evaluations(), EVALUATIONS_PER_WORKER * multi_candidates);
        let shares: u64 = d.iter().map(|(by, _)| d.share_ppm(by)).sum();
        assert!(shares <= 1_000_000 && shares + 12 >= 1_000_000, "{shares}");
        assert!(
            d.iter().any(|(by, _)| matches!(by, DecidedBy::Key(_))),
            "the stream must exercise the keys"
        );
        assert_eq!(r.placements().len(), 600);
    }
    // Ranker 5 here is the adapter's ranker 5.
    let mut adapter = ScoreRanker::new();
    let five = run(RankerKind::Score, &rounds, None);
    let adapter_seq: Vec<(u64, WorkerId)> = rounds
        .iter()
        .map(|r| {
            let c = r.names();
            (
                r.lease.lease_ix,
                adapter.place(&r.model(), &c).expect("host").clone(),
            )
        })
        .collect();
    assert_eq!(five.placements(), adapter_seq.as_slice());
    assert_eq!(five.evaluations(), adapter.evaluations());
}

// ---- GD counters ----

/// Shows: `scored_long` counts scored placements of a lease with `L >= 2` and no others;
/// the guard names the first floor that fails, and each floor fails alone; `differing`
/// counts hosts that differ by lease index and counts a lease the other did not place.
/// Catches a guard that cannot fail and a counter on the wrong condition.
#[test]
fn gd_counters_and_guard() {
    let hosts = [("p", host([30_000; SLOTS])), ("q", host([40_000; SLOTS]))];
    let mut r = Ranker::new(RankerKind::Score, base());
    for (ix, l, n) in [(1, 1, 2), (2, 3, 2), (3, 3, 1), (4, 2, 2)] {
        let m = model(lease(ix, l, [NEW; SLOTS]), &hosts[..n]);
        let c: Vec<WorkerId> = hosts[..n].iter().map(|(n, _)| id(n)).collect();
        r.place(&m, &c);
    }
    assert_eq!((r.attempts(), r.scored(), r.scored_long()), (4, 3, 2));

    let ok = Gd {
        placements: 10_000,
        scored: 2_500,
        scored_long: 600,
        differ: 200,
    };
    assert_eq!(ok.check(10_000), Ok(()));
    // Each floor is inclusive: a run exactly at it passes.
    let at_floors = Gd {
        placements: 10_000,
        scored: 2_000,
        scored_long: 500,
        differ: 100,
    };
    assert_eq!(at_floors.check(10_000), Ok(()));
    let fails = [
        (
            Gd {
                scored: 1_999,
                scored_long: 600,
                ..ok
            },
            "scored placements below the share of placements",
        ),
        (
            Gd {
                placements: 100_000,
                scored_long: 6_000,
                ..ok
            },
            "scored placements below the share of placements",
        ),
        (
            Gd {
                placements: 5_000,
                scored: 1_999,
                scored_long: 600,
                ..ok
            },
            "too few scored placements",
        ),
        (
            Gd {
                scored_long: 499,
                ..ok
            },
            "scored placements with L >= 2 below the share of placements",
        ),
        (
            Gd { differ: 99, ..ok },
            "placement sequences differ in too few placements",
        ),
    ];
    for (gd, why) in fails {
        assert_eq!(gd.check(10_000), Err(why), "{gd:?}");
    }
    assert_eq!(
        (GD_SCORED_PPM, GD_SCORED_MIN, GD_LONG_PPM),
        (200_000, 2_000, 50_000)
    );
    assert_eq!(ratio_ppm(1, 3), 333_333);
    assert_eq!(ratio_ppm(5, 0), 0);
    assert_eq!(percent(250_000), "25.0000%");
    assert_eq!(percent(1_234), "0.1234%");

    let seq = |v: &[(u64, &str)]| -> Vec<(u64, WorkerId)> {
        v.iter().map(|(i, n)| (*i, id(n))).collect()
    };
    let a = seq(&[(1, "p"), (2, "q"), (3, "p")]);
    let b = seq(&[(1, "p"), (2, "p")]);
    assert_eq!(differing(&a, &b), 2, "one host differs, one lease missing");
    assert_eq!(differing(&a, &a), 0);
    assert_ne!(sequence_hash(&a), sequence_hash(&b));
    assert_ne!(
        sequence_hash(&seq(&[(1, "p")])),
        sequence_hash(&seq(&[(2, "p")]))
    );
    assert_ne!(
        sequence_hash(&seq(&[(1, "p"), (2, "q")])),
        sequence_hash(&seq(&[(2, "q"), (1, "p")]))
    );
    assert_ne!(
        sequence_hash(&seq(&[(1, "ab"), (2, "c")])),
        sequence_hash(&seq(&[(1, "a"), (2, "bc")]))
    );
}

/// Shows: the three scorer rankers decide differently from one another on a seeded
/// stream (the counts the report will carry for ranker 5 against ranker 3 and ranker 4),
/// printed with `--nocapture`; the stream is synthetic, so the figures say the counters
/// work, not how often the rankers differ on the replayed day. Catches counters that
/// compare a ranker with itself.
#[test]
fn gd_of_ranker_five_against_three_and_four_is_printed() {
    let rounds = stream(4_000, 21);
    let r3 = run(RankerKind::Linear, &rounds, None);
    let r4 = run(RankerKind::Flat, &rounds, None);
    let r5 = run(RankerKind::Score, &rounds, None);
    let g53 = Gd::of(&r5, &r3);
    let g54 = Gd::of(&r5, &r4);
    println!("{}", g53.line("R5 vs R3 (seeded stream, synthetic)"));
    println!("{}", g54.line("R5 vs R4 (seeded stream, synthetic)"));
    assert!(g53.differ > 0 && g54.differ > 0);
    assert_eq!(Gd::of(&r5, &r5).differ, 0);
    assert_eq!(g53.placements, 4_000);
}

// ---- A7: candidate order ----

/// Shows: handing the candidates in a seeded per-round shuffle gives the same placement
/// sequence (same hash), the same sequence of deciding rules and the same evaluation
/// count as name order, for rankers 3, 4, 5 and FW5; and the check can fail, because a
/// picker that takes the first candidate gives a different sequence under the shuffle.
/// Catches a ranker that breaks ties by the position in the candidate list. Ranker 2
/// (random feasible) and ranker 1 (least loaded) are not on this branch; their order
/// checks use the same hash.
#[test]
fn candidate_order_does_not_change_the_placements() {
    let rounds = stream(800, 33);
    for kind in KINDS {
        let by_name = run(kind, &rounds, None);
        for seed in [1, 2, 3] {
            let shuffled = run(kind, &rounds, Some(seed));
            assert_eq!(
                by_name.sequence_hash(),
                shuffled.sequence_hash(),
                "{kind:?} {seed}"
            );
            assert_eq!(by_name.placements(), shuffled.placements());
            assert_eq!(by_name.decisions(), shuffled.decisions());
            assert_eq!(by_name.evaluations(), shuffled.evaluations());
        }
    }
    let first_of = |shuffle: Option<u64>| {
        let mut s = shuffle.unwrap_or(0);
        let picks: Vec<(u64, WorkerId)> = rounds
            .iter()
            .map(|round| {
                let mut c = round.names();
                if shuffle.is_some() {
                    for i in (1..c.len()).rev() {
                        c.swap(i, below(&mut s, i as u64 + 1) as usize);
                    }
                }
                (round.lease.lease_ix, c[0].clone())
            })
            .collect();
        sequence_hash(&picks)
    };
    assert_ne!(first_of(None), first_of(Some(1)));
}

// ---- kinds and FW5 ----

/// Shows: each kind maps to its fit, only FW5 asks for flattened beliefs, the base flags
/// are kept, and `flat_belief` charges the same at every slot with `L = 1`, keeping the
/// index, the cold flag and `p_bar`. On flat curves FW5 is ranker 5 (it is the same scorer
/// on flat input). Catches a swapped mapping and a flattened belief that keeps a curve.
#[test]
fn kinds_map_to_fits_and_fw5_takes_flat_beliefs() {
    let b = ScoreConfig::new(2, 5_000, 123_456).expect("in range");
    assert_eq!(RankerKind::Score.config(b), b);
    assert_eq!(RankerKind::FlatScore.config(b), b);
    assert_eq!(RankerKind::Linear.config(b), b.with_fit(FitShape::Linear));
    assert_eq!(RankerKind::Flat.config(b), b.with_fit(FitShape::Flat));
    assert_ne!(RankerKind::Linear.config(b), b);
    let fw: Vec<RankerKind> = KINDS.into_iter().filter(|k| k.flattens_beliefs()).collect();
    assert_eq!(fw, [RankerKind::FlatScore]);
    for k in KINDS {
        assert!(!k.label().is_empty());
    }

    let mut curve = [0; SLOTS];
    for (i, c) in curve.iter_mut().enumerate() {
        *c = 3_000 + 100 * i as u64;
    }
    let mut l = lease(77, 9, curve);
    l.cold = true;
    let flat = flat_belief(&l, cm(4_000));
    assert_eq!(flat.charge, [cm(4_000); SLOTS]);
    assert_eq!(flat.fit_minutes, 1);
    assert_eq!(
        (flat.lease_ix, flat.cold, flat.p_bar_cpu_millis),
        (77, true, 3_000)
    );

    let flat_world = [
        ("a", host([20_000; SLOTS])),
        ("b", host([55_000; SLOTS])),
        ("c", host([55_000; SLOTS])),
    ];
    let m = model(flat_belief(&l, cm(4_000)), &flat_world);
    let c = names(&["a", "b", "c"]);
    let mut r5 = Ranker::new(RankerKind::Score, base());
    let mut fw5 = Ranker::new(RankerKind::FlatScore, base());
    assert_eq!(r5.place(&m, &c), fw5.place(&m, &c));
    assert_eq!(r5.decisions(), fw5.decisions());
}
