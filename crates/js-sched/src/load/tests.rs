use std::collections::BTreeMap;

use super::*;

const BIG_MEMORY: u64 = 1 << 40;

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn names(list: &[&str]) -> Vec<WorkerId> {
    list.iter().map(|n| worker(n)).collect()
}

#[derive(Clone)]
struct Host {
    facts: HostFacts,
    load: [Charge; SLOTS],
    new: [Charge; SLOTS],
}

impl Host {
    /// A worker of `cpu` millicores and `memory` bytes, carrying and asked for nothing.
    fn empty(cpu: u64, memory: u64) -> Self {
        Self {
            facts: HostFacts {
                cpu_capacity_millis: cpu,
                memory_capacity_bytes: memory,
                fit_minutes: 1,
                fallback: false,
                measured_above_predicted: false,
                same_key_millis: 0,
                new_key_millis: 0,
                uncovered_leases: 0,
            },
            load: [Charge::default(); SLOTS],
            new: [Charge::default(); SLOTS],
        }
    }

    fn cpu_load(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.load.iter_mut().enumerate() {
            c.cpu_millis = f(slot);
        }
        self
    }

    fn cpu_new(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.new.iter_mut().enumerate() {
            c.cpu_millis = f(slot);
        }
        self
    }

    fn mem_load(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.load.iter_mut().enumerate() {
            c.memory_bytes = f(slot);
        }
        self
    }

    fn mem_new(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.new.iter_mut().enumerate() {
            c.memory_bytes = f(slot);
        }
        self
    }

    fn life(mut self, minutes: u8) -> Self {
        self.facts.fit_minutes = minutes;
        self
    }
}

struct Fake {
    lease: LeaseFacts,
    hosts: BTreeMap<WorkerId, Host>,
}

impl Fake {
    fn new(hosts: Vec<(&str, Host)>) -> Self {
        Self {
            lease: LeaseFacts {
                lease: LeaseId::new(1, 2),
                cold: false,
                retry: false,
            },
            hosts: hosts.into_iter().map(|(n, h)| (worker(n), h)).collect(),
        }
    }
}

impl PredictedLoad for Fake {
    fn lease(&self) -> LeaseFacts {
        self.lease
    }

    fn load(&self, worker: &WorkerId, slot: usize) -> Charge {
        self.hosts[worker].load[slot]
    }

    fn charge(&self, worker: &WorkerId, slot: usize) -> Charge {
        self.hosts[worker].new[slot]
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        self.hosts[worker].facts
    }
}

fn key(fake: &Fake, name: &str) -> ScoreKey {
    Scorer::default().key(fake, &worker(name))
}

fn pick(fake: &Fake, order: &[&str]) -> String {
    let candidates = names(order);
    let mut scorer = Scorer::default();
    scorer
        .choose(fake, &candidates)
        .expect("a candidate")
        .as_str()
        .to_owned()
}

// The design's example: two 88-thread hosts. A runs 60 cores in minutes 0 to 4 (dilated)
// and 10 after; B runs a flat 30. The new lease is 24 cores when quiet and 60 in minutes
// 5 to 15.
fn example() -> Fake {
    let new = |slot: usize| {
        if (5..=15).contains(&slot) {
            60_000
        } else {
            24_000
        }
    };
    let a = Host::empty(88_000, BIG_MEMORY)
        .cpu_load(|s| if s <= 4 { 60_000 } else { 10_000 })
        .cpu_new(new)
        .life(16);
    let b = Host::empty(88_000, BIG_MEMORY)
        .cpu_load(|_| 30_000)
        .cpu_new(new)
        .life(16);
    Fake::new(vec![("a", a), ("b", b)])
}

#[test]
fn the_design_example_goes_to_the_host_with_the_idle_gap() {
    let fake = example();
    assert_eq!(pick(&fake, &["a", "b"]), "a");
    assert_eq!(pick(&fake, &["b", "a"]), "a");
}

#[test]
fn the_design_example_residuals_in_parts_per_million() {
    // Rounded down: the design's text rounds to nearest (45,455 and 386,364).
    assert_eq!(residual(88_000, 60_000, 24_000), (45_454, 0));
    assert_eq!(residual(88_000, 10_000, 60_000), (204_545, 0));
    assert_eq!(residual(88_000, 30_000, 24_000), (386_363, 0));
    assert_eq!(residual(88_000, 30_000, 60_000), (0, 22_727));
}

#[test]
fn the_design_example_keys() {
    let fake = example();
    let a = key(&fake, "a");
    let b = key(&fake, "b");
    // A is never above 88 cores.
    assert_eq!(a.slack, 0);
    // B is 22,727 ppm over at minutes 5 to 15, 12,727 above the tolerance, weights 26 to 16.
    assert_eq!(b.slack, (16..=26u128).sum::<u128>() * 12_727);
    // The fit over the lease's 16 minutes; memory is untouched, so the leftover of that
    // resource is the whole of it and the difference to the binding one is stranded.
    let fit = |per_slot: &dyn Fn(usize) -> u128| -> u128 {
        (0..16)
            .map(|slot| {
                let x = per_slot(slot);
                weight(slot) * (2 * x * x + (1_000_000 - x) * (1_000_000 - x))
            })
            .sum()
    };
    assert_eq!(a.fit, fit(&|s| if s <= 4 { 45_454 } else { 204_545 }));
    assert_eq!(b.fit, fit(&|s| if s <= 4 { 386_363 } else { 0 }));
}

#[test]
fn a_lighter_load_now_does_not_win_over_a_better_fit() {
    // The example host that is emptier now (B, 30 of 88 cores against A's 60) loses.
    let fake = example();
    assert!(
        fake.hosts[&worker("b")].load[0].cpu_millis < fake.hosts[&worker("a")].load[0].cpu_millis
    );
    assert!(key(&fake, "a") < key(&fake, "b"));
}

fn at_slot(slot: usize, v: u64) -> impl Fn(usize) -> u64 {
    move |s| if s == slot { v } else { 0 }
}

#[test]
fn near_overshoot_costs_more_than_far_overshoot() {
    // Both are 5% over at one slot; the one at minute 2 weighs 29, the one at minute 20, 11.
    let near = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(2, 105_000));
    let far = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(20, 105_000));
    let fake = Fake::new(vec![("near", near), ("far", far)]);
    assert_eq!(key(&fake, "near").slack, 29 * (50_000 - 10_000));
    assert_eq!(key(&fake, "far").slack, 11 * (50_000 - 10_000));
    assert_eq!(pick(&fake, &["near", "far"]), "far");
}

#[test]
fn the_overshoot_zone_is_used_only_when_a_host_fits_inside_100_percent() {
    let over = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(29, 105_000));
    let inside = Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 99_000);
    let fake = Fake::new(vec![("over", over), ("inside", inside)]);
    assert_eq!(pick(&fake, &["over", "inside"]), "inside");
    assert_eq!(key(&fake, "inside").slack, 0);
}

#[test]
fn overshoot_within_the_tolerance_is_ignored() {
    // 1% over is exactly the default tolerance: no slack; a little more is slack.
    let at_tol = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(0, 101_000));
    let above = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(0, 101_100));
    let fake = Fake::new(vec![("at", at_tol), ("above", above)]);
    assert_eq!(key(&fake, "at").slack, 0);
    assert_eq!(key(&fake, "above").slack, 31 * 1_000);
}

#[test]
fn the_tolerance_is_a_flag() {
    let host = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(0, 101_000));
    let fake = Fake::new(vec![("h", host)]);
    let strict = ScoreConfig::new(1, 0, 250_000).unwrap();
    let k = Scorer::new(strict).key(&fake, &worker("h"));
    assert_eq!(k.slack, 31 * 10_000);
}

#[test]
fn memory_overshoot_counts_as_slack_too() {
    let host = Host::empty(100_000, 1_000).mem_load(at_slot(3, 1_100));
    let fake = Fake::new(vec![("h", host)]);
    // 10% over in memory at minute 3 (weight 28), 90,000 above the 10,000 tolerance.
    assert_eq!(key(&fake, "h").slack, 28 * 90_000);
}

#[test]
fn the_h_plus_slot_is_scored_by_the_safety_keys_with_weight_one() {
    let host = Host::empty(100_000, BIG_MEMORY).cpu_load(at_slot(H_PLUS, 105_000));
    let fake = Fake::new(vec![("h", host)]);
    assert_eq!(key(&fake, "h").slack, 40_000);
    assert_eq!(weight(H_PLUS), 1);
    assert_eq!(weight(0), 31);
    assert_eq!(weight(HORIZON), 1);
}

#[test]
fn the_fit_looks_only_at_the_leading_minutes_of_the_lease() {
    // The hosts differ at minute 10 and at H+, below 100%, so slack is 0 for both.
    let base = || Host::empty(100_000, BIG_MEMORY);
    let quiet = base();
    let busy10 = base().cpu_load(at_slot(10, 50_000));
    let busy_h = base().cpu_load(at_slot(H_PLUS, 50_000));
    let fake = Fake::new(vec![("q", quiet), ("b10", busy10), ("bh", busy_h)]);
    assert_eq!(key(&fake, "q").fit, key(&fake, "bh").fit);
    assert_eq!(key(&fake, "q").fit, key(&fake, "b10").fit);

    let long = |h: Host| h.life(11);
    let fake = Fake::new(vec![
        ("q", long(base())),
        ("b10", long(base().cpu_load(at_slot(10, 50_000)))),
        ("bh", long(base().cpu_load(at_slot(H_PLUS, 50_000)))),
    ]);
    assert_ne!(key(&fake, "q").fit, key(&fake, "b10").fit);
    assert_eq!(key(&fake, "q").fit, key(&fake, "bh").fit);
}

#[test]
fn the_life_in_minutes_is_clamped_to_one_through_the_horizon() {
    let at = |life: u8| {
        let h = Host::empty(100_000, BIG_MEMORY)
            .cpu_load(|_| 50_000)
            .life(life);
        key(&Fake::new(vec![("h", h)]), "h").fit
    };
    assert_eq!(at(0), at(1));
    assert_eq!(at(HORIZON as u8), at(255));
    assert_ne!(at(1), at(2));
    assert_ne!(at(29), at(30));
    // Minutes 0 to 29 with weights 31 down to 2: the same leftover in every slot.
    let x = 500_000u128;
    let per_slot = 2 * x * x + (1_000_000 - x) * (1_000_000 - x);
    assert_eq!(at(30), (2..=31u128).sum::<u128>() * per_slot);
}

#[test]
fn the_fit_squares_the_binding_leftover_and_counts_the_stranded_one_by_balance() {
    // Slot 0 only (life 1): CPU leftover 40%, memory leftover 10%; memory binds.
    let host = Host::empty(1_000, 1_000)
        .cpu_load(at_slot(0, 600))
        .mem_load(at_slot(0, 900))
        .life(1);
    let fake = Fake::new(vec![("h", host)]);
    let fit = |b: u32| {
        let c = ScoreConfig::new(b, 10_000, 250_000).unwrap();
        Scorer::new(c).key(&fake, &worker("h")).fit
    };
    let binding = 2 * 100_000u128 * 100_000;
    let stranded = 300_000u128 * 300_000;
    assert_eq!(fit(0), 31 * binding);
    assert_eq!(fit(1), 31 * (binding + stranded));
    assert_eq!(fit(4), 31 * (binding + 4 * stranded));
}

#[test]
fn the_balance_range_stops_at_four() {
    assert!(ScoreConfig::new(4, 0, 0).is_some());
    assert!(ScoreConfig::new(5, 0, 0).is_none());
    let d = ScoreConfig::default();
    assert_eq!(d, ScoreConfig::new(1, 10_000, 250_000).unwrap());
}

fn memory_unsafe(cold: bool, lm: u64, nm: u64, slot: usize) -> bool {
    let host = Host::empty(100_000, 1_000)
        .mem_load(at_slot(slot, lm))
        .mem_new(at_slot(slot, nm));
    let mut fake = Fake::new(vec![("h", host)]);
    fake.lease.cold = cold;
    key(&fake, "h").memory_unsafe
}

#[test]
fn a_warm_key_must_fit_one_point_three_times() {
    // ceil(769 * 1.3) = 1000, ceil(770 * 1.3) = 1001.
    assert!(!memory_unsafe(false, 0, 769, 0));
    assert!(memory_unsafe(false, 0, 770, 0));
    // The load adds as is.
    assert!(!memory_unsafe(false, 100, 692, 0));
    assert!(memory_unsafe(false, 101, 692, 0));
}

#[test]
fn a_cold_key_must_fit_one_point_six_three_times() {
    // ceil(613 * 1.63) = 1000, ceil(614 * 1.63) = 1001.
    assert!(!memory_unsafe(true, 0, 613, 0));
    assert!(memory_unsafe(true, 0, 614, 0));
}

/// Catches: a retry of a warm key held to the warm factor (its last run missed, so it is
/// held to the cold one): 614 bytes fit 1.3 times in 1,000 but not 1.63 times.
#[test]
fn a_retry_must_fit_one_point_six_three_times() {
    let unsafe_as = |cold: bool, retry: bool, nm: u64| {
        let host = Host::empty(100_000, 1_000).mem_new(at_slot(0, nm));
        let mut fake = Fake::new(vec![("h", host)]);
        fake.lease.cold = cold;
        fake.lease.retry = retry;
        key(&fake, "h").memory_unsafe
    };
    assert!(!unsafe_as(false, false, 614));
    assert!(!unsafe_as(false, true, 613));
    assert!(unsafe_as(false, true, 614));
    assert!(unsafe_as(true, true, 614));
}

#[test]
fn memory_unsafe_looks_at_every_slot_including_h_plus() {
    assert!(memory_unsafe(false, 0, 770, 17));
    assert!(memory_unsafe(false, 0, 770, H_PLUS));
    assert!(!memory_unsafe(false, 0, 769, H_PLUS));
}

fn unsafe_vs_better_fit(unsafe_load: u64) -> String {
    // U is fuller in CPU (a tighter fit) and holds `unsafe_load` bytes of 1,000; S is empty.
    let new = |h: Host| h.cpu_new(|_| 10_000).mem_new(|_| 600);
    let u = new(Host::empty(100_000, 1_000)
        .cpu_load(|_| 80_000)
        .mem_load(|_| unsafe_load));
    let s = new(Host::empty(100_000, 1_000));
    let fake = Fake::new(vec![("u", u), ("s", s)]);
    pick(&fake, &["u", "s"])
}

#[test]
fn a_host_that_cannot_take_a_memory_miss_loses_to_a_looser_one_that_can() {
    // 250 + ceil(1.3 * 600) = 1030 > 1000.
    assert_eq!(unsafe_vs_better_fit(250), "s");
    // 200 + 780 = 980: safe, so the tighter fit wins.
    assert_eq!(unsafe_vs_better_fit(200), "u");
}

/// `over` is 5% above its CPU capacity at minute 0 (CPU slack, memory safe); the other is
/// inside 100% and holds `other_memory` of 1,000 bytes. The lease asks 10,000 millicores
/// and 600 bytes, so a memory miss needs 780 bytes of room.
fn slack_vs_memory(other_memory: u64) -> (String, bool, bool) {
    let new = |h: Host| h.cpu_new(|_| 10_000).mem_new(|_| 600);
    let over = new(Host::empty(100_000, 1_000).cpu_load(at_slot(0, 95_000)));
    let inside = new(Host::empty(100_000, 1_000)
        .cpu_load(|_| 50_000)
        .mem_load(|_| other_memory));
    let fake = Fake::new(vec![("over", over), ("inside", inside)]);
    let (over_key, inside_key) = (key(&fake, "over"), key(&fake, "inside"));
    assert!(over_key.slack > 0 && !over_key.memory_unsafe);
    assert_eq!(inside_key.slack, 0);
    (
        pick(&fake, &["over", "inside"]),
        inside_key.memory_unsafe,
        pick(&fake, &["inside", "over"]) == pick(&fake, &["over", "inside"]),
    )
}

#[test]
fn cpu_slack_outranks_the_memory_miss() {
    // The host inside 100% cannot take the miss (250 + 780 > 1,000) and the host with CPU
    // slack can: slack is key 1 and the miss key 2, so the host inside 100% wins.
    assert_eq!(slack_vs_memory(250), ("inside".to_owned(), true, true));
    // Control: with room for the miss on both, slack alone decides the same way.
    assert_eq!(slack_vs_memory(200), ("inside".to_owned(), false, true));
}

#[test]
fn a_distrusted_host_loses_to_a_trusted_one_with_a_worse_fit() {
    let full = |f: fn(&mut HostFacts)| {
        let mut h = Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 80_000);
        f(&mut h.facts);
        h
    };
    let trusted = Host::empty(100_000, BIG_MEMORY);
    let fake = Fake::new(vec![
        ("fallback", full(|f| f.fallback = true)),
        ("lying", full(|f| f.measured_above_predicted = true)),
        ("trusted", trusted),
    ]);
    assert!(key(&fake, "fallback").distrusted);
    assert!(key(&fake, "lying").distrusted);
    assert!(!key(&fake, "trusted").distrusted);
    assert_eq!(pick(&fake, &["fallback", "lying", "trusted"]), "trusted");
}

#[test]
fn distrust_outranks_concentration() {
    let mut distrusted = Host::empty(100_000, BIG_MEMORY);
    distrusted.facts.fallback = true;
    let mut concentrated = Host::empty(100_000, BIG_MEMORY);
    concentrated.facts.same_key_millis = 60_000;
    let fake = Fake::new(vec![("d", distrusted), ("c", concentrated)]);
    assert_eq!(pick(&fake, &["d", "c"]), "c");
}

#[test]
fn concentration_is_a_share_of_the_cpu_capacity_strictly_exceeded() {
    let with = |same: u64, new: u64| {
        let mut h = Host::empty(100_000, BIG_MEMORY);
        h.facts.same_key_millis = same;
        h.facts.new_key_millis = new;
        key(&Fake::new(vec![("h", h)]), "h").concentrated
    };
    // 25% of 100 cores is 25,000 millicores.
    assert!(!with(15_000, 10_000));
    assert!(with(15_000, 10_001));
    assert!(!with(0, 0));
}

#[test]
fn when_every_host_is_concentrated_the_fit_decides() {
    let host = |load: u64| {
        let mut h = Host::empty(100_000, BIG_MEMORY).cpu_load(|_| load);
        h.facts.same_key_millis = 90_000;
        h
    };
    let fake = Fake::new(vec![("loose", host(10_000)), ("tight", host(50_000))]);
    assert!(key(&fake, "loose").concentrated && key(&fake, "tight").concentrated);
    assert_eq!(pick(&fake, &["loose", "tight"]), "tight");
}

#[test]
fn concentration_outranks_the_fit() {
    let mut concentrated = Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 50_000);
    concentrated.facts.same_key_millis = 80_000;
    let spread = Host::empty(100_000, BIG_MEMORY);
    let fake = Fake::new(vec![("c", concentrated), ("s", spread)]);
    assert_eq!(pick(&fake, &["c", "s"]), "s");
}

// Equal on keys 1 to 5, hosts differ in one thing.
fn twins() -> (Host, Host) {
    let h = Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 20_000);
    (h.clone(), h)
}

#[test]
fn ties_go_to_the_host_with_fewer_uncovered_leases() {
    let (mut a, b) = twins();
    a.facts.uncovered_leases = 3;
    let fake = Fake::new(vec![("a", a), ("b", b)]);
    assert_eq!(pick(&fake, &["a", "b"]), "b");
    assert_eq!(pick(&fake, &["b", "a"]), "b");
}

#[test]
fn then_to_the_larger_host() {
    // The same fractions on a twice larger host: the keys before the tie-breaks are equal.
    let small = Host::empty(100_000, BIG_MEMORY)
        .cpu_load(|_| 20_000)
        .cpu_new(|_| 10_000);
    let large = Host::empty(200_000, BIG_MEMORY)
        .cpu_load(|_| 40_000)
        .cpu_new(|_| 20_000);
    let fake = Fake::new(vec![("small", small), ("large", large)]);
    let (s, l) = (key(&fake, "small"), key(&fake, "large"));
    assert_eq!((s.slack, s.fit), (l.slack, l.fit));
    assert_eq!(pick(&fake, &["small", "large"]), "large");
}

#[test]
fn then_to_the_larger_headroom_over_the_next_five_minutes() {
    // Only minute 3 differs and is within the five; life 1 keeps it out of the fit.
    let (a, b) = twins();
    let a = a.cpu_load(|s| if s == 3 { 30_000 } else { 20_000 });
    let fake = Fake::new(vec![("a", a), ("b", b)]);
    assert_eq!(pick(&fake, &["a", "b"]), "b");
    // Minute 5 is not within the five: the hash decides.
    let (a, b) = twins();
    let a = a.cpu_load(|s| if s == 5 { 30_000 } else { 20_000 });
    let fake = Fake::new(vec![("a", a), ("b", b)]);
    assert_eq!(pick(&fake, &["a", "b"]), by_hash(&fake, "a", "b"));
}

fn by_hash(fake: &Fake, a: &str, b: &str) -> String {
    let id = fake.lease.lease;
    let (ha, hb) = (tie_hash(id, &worker(a)), tie_hash(id, &worker(b)));
    assert_ne!(ha, hb);
    (if ha < hb { a } else { b }).to_owned()
}

#[test]
fn the_last_tie_break_is_the_hash_of_lease_and_worker_not_the_order() {
    let (a, b) = twins();
    let mut fake = Fake::new(vec![("a", a), ("b", b)]);
    let mut winners = std::collections::BTreeSet::new();
    for seq in 0..32 {
        fake.lease.lease = LeaseId::new(1, seq);
        let w = pick(&fake, &["a", "b"]);
        assert_eq!(w, pick(&fake, &["b", "a"]));
        assert_eq!(w, by_hash(&fake, "a", "b"));
        winners.insert(w);
    }
    // Equal scores spread by lease: both hosts win some.
    assert_eq!(winners.len(), 2);
}

#[test]
fn the_hash_is_fnv1a_over_length_prefixed_lease_and_worker() {
    // Pinned vectors: 64-bit FNV-1a over `len as u32 LE, bytes` for the lease text, then
    // the worker name.
    assert_eq!(fnv1a(&[b"1-2", b"w1"]), 0x922e_fe9b_031d_ce50);
    assert_eq!(fnv1a(&[b"ab", b"c"]), 0x6f7c_37fe_7f9f_3b56);
    assert_eq!(fnv1a(&[b"a", b"bc"]), 0xa49d_b729_a899_2b5a);
    assert_eq!(fnv1a(&[b"", b""]), 0xa8c7_f832_281a_39c5);
    assert_eq!(
        tie_hash(LeaseId::new(1, 2), &worker("w1")),
        0x922e_fe9b_031d_ce50
    );
}

#[test]
fn one_candidate_is_taken_without_scoring_and_none_gives_none() {
    let (a, b) = twins();
    let fake = Fake::new(vec![("a", a), ("b", b)]);
    let mut scorer = Scorer::default();
    assert_eq!(scorer.choose(&fake, &names(&["b"])), Some(&worker("b")));
    assert_eq!(scorer.choose(&fake, &[]), None);
    assert_eq!(scorer.evaluations(), 0);
}

#[test]
fn evaluations_are_64_per_scored_worker_per_request() {
    let hosts = ["a", "b", "c"].map(|n| (n, Host::empty(100_000, BIG_MEMORY)));
    let fake = Fake::new(hosts.to_vec());
    let mut scorer = Scorer::default();
    let all = names(&["a", "b", "c"]);
    scorer.choose(&fake, &all);
    assert_eq!(scorer.evaluations(), 3 * 64);
    // A worker the caller left out is never evaluated.
    scorer.choose(&fake, &all[..2]);
    assert_eq!(scorer.evaluations(), 3 * 64 + 2 * 64);
    // One left: nothing is scored.
    scorer.choose(&fake, &all[..1]);
    assert_eq!(scorer.evaluations(), 5 * 64);
}

#[test]
fn extreme_values_neither_overflow_nor_divide_by_zero() {
    assert_eq!(residual(0, 5, 5), (0, 9 * 1_000_000));
    assert_eq!(residual(0, 0, 0), (1_000_000, 0));
    let (x, o) = residual(1, u64::MAX, u64::MAX);
    assert_eq!(x, 0);
    assert_eq!(o, (2 * u128::from(u64::MAX) - 1) * 1_000_000);

    let mut huge = Host::empty(u64::MAX, u64::MAX)
        .cpu_load(|_| u64::MAX)
        .cpu_new(|_| u64::MAX)
        .mem_load(|_| u64::MAX)
        .mem_new(|_| u64::MAX)
        .life(255);
    huge.facts.same_key_millis = u64::MAX;
    huge.facts.new_key_millis = u64::MAX;
    let zero = Host::empty(0, 0)
        .cpu_load(|_| u64::MAX)
        .mem_new(|_| u64::MAX);
    let fake = Fake::new(vec![("huge", huge), ("zero", zero)]);
    let (h, z) = (key(&fake, "huge"), key(&fake, "zero"));
    assert!(h.slack > 0 && z.slack > 0 && h.memory_unsafe && z.memory_unsafe);
    assert!(h.concentrated);
}

/// A small deterministic generator (xorshift64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A random worker: a capacity, a load curve of a few levels and a lease charge that is a
/// flat quiet level with one busy stretch.
fn random_host(rng: &mut Rng) -> Host {
    let cpu = [16_000, 88_000, 32_000][rng.below(3) as usize];
    let mem = cpu << 20;
    let levels: Vec<u64> = (0..4).map(|_| rng.below(105) * cpu / 100).collect();
    let edge = rng.below(SLOTS as u64) as usize;
    let quiet = rng.below(30) * cpu / 100;
    let busy = rng.below(60) * cpu / 100;
    let (from, to) = (rng.below(20) as usize, rng.below(20) as usize + 20);
    let mut h = Host::empty(cpu, mem)
        .cpu_load(|s| levels[usize::from(s > edge) + 2 * usize::from(s > 10)])
        .cpu_new(|s| if (from..to).contains(&s) { busy } else { quiet })
        .life(u8::try_from(1 + rng.below(30)).unwrap());
    let (load, new) = (h.load, h.new);
    h = h
        .mem_load(|s| load[s].cpu_millis << 20)
        .mem_new(|s| new[s].cpu_millis << 20);
    h.facts.uncovered_leases = u32::try_from(rng.below(4)).unwrap();
    h.facts.fallback = rng.below(5) == 0;
    h
}

/// The predicate stand-in: every slot within 110% of both capacities.
fn admitted(h: &Host) -> bool {
    (0..SLOTS).all(|s| {
        let c = u128::from(h.load[s].cpu_millis) + u128::from(h.new[s].cpu_millis);
        let m = u128::from(h.load[s].memory_bytes) + u128::from(h.new[s].memory_bytes);
        c * 10 <= u128::from(h.facts.cpu_capacity_millis) * 11
            && m <= u128::from(h.facts.memory_capacity_bytes) * 11 / 10
    })
}

#[test]
fn the_choice_is_always_an_admitted_candidate_and_does_not_depend_on_their_order() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut checked, mut empty, mut inside, mut forced) = (0, 0, 0, 0);
    for round in 0..300 {
        let count = 2 + rng.below(7) as usize;
        let all: Vec<(String, Host)> = (0..count)
            .map(|i| (format!("w{i}"), random_host(&mut rng)))
            .collect();
        let fake = Fake::new(all.iter().map(|(n, h)| (n.as_str(), h.clone())).collect());
        let mut fake = fake;
        fake.lease.lease = LeaseId::new(1, round);
        fake.lease.cold = round % 2 == 0;
        let ok: Vec<WorkerId> = all
            .iter()
            .filter(|(_, h)| admitted(h))
            .map(|(n, _)| worker(n))
            .collect();
        if ok.is_empty() {
            empty += 1;
            continue;
        }
        checked += 1;
        let mut scorer = Scorer::default();
        let first = scorer.choose(&fake, &ok).unwrap().clone();
        assert!(ok.contains(&first), "chose a worker that was not offered");

        // Same input, same choice; any order of the candidates, same choice.
        assert_eq!(scorer.choose(&fake, &ok), Some(&first));
        let mut shuffled = ok.clone();
        for _ in 0..4 {
            let n = shuffled.len();
            for i in (1..n).rev() {
                shuffled.swap(i, rng.below(i as u64 + 1) as usize);
            }
            assert_eq!(scorer.choose(&fake, &shuffled), Some(&first));
        }

        // A host that fits inside 100% at every slot (within the tolerance) is never
        // passed over for one that needs the overshoot zone.
        let slack = |w: &WorkerId| scorer.clone().key(&fake, w).slack;
        if ok.iter().any(|w| slack(w) == 0) {
            inside += 1;
            assert_eq!(slack(&first), 0);
        } else {
            forced += 1;
        }
    }
    assert!(checked > 100, "the generator admits too few worker sets");
    assert!(
        empty > 0 && inside > 0 && forced > 0,
        "the generator misses a case"
    );
}

#[test]
fn the_score_flags_default_and_stay_in_range() {
    let cfg = crate::LoadConfig::default();
    assert_eq!(
        (
            cfg.score_balance,
            cfg.score_slack_tol_ppm,
            cfg.key_share_percent
        ),
        (1, 10_000, 25)
    );
    assert_eq!(ScoreConfig::of(&cfg), ScoreConfig::default());
    let wild = crate::LoadConfig {
        score_balance: 99,
        key_share_percent: 500,
        ..cfg
    };
    assert_eq!(
        ScoreConfig::of(&wild),
        ScoreConfig::new(4, 10_000, 1_000_000).unwrap()
    );
}

#[test]
fn a_chosen_worker_says_how_it_was_decided() {
    // `a` fits better but is concentrated; `b` is empty. The concentrated key decides for b.
    let mut a = Host::empty(100_000, BIG_MEMORY)
        .cpu_load(|_| 50_000)
        .cpu_new(|_| 10_000);
    a.facts.same_key_millis = 20_000;
    a.facts.new_key_millis = 10_000;
    let b = Host::empty(100_000, BIG_MEMORY).cpu_new(|_| 10_000);
    let fake = Fake::new(vec![("a", a), ("b", b)]);
    let all = names(&["a", "b"]);
    let refs: Vec<&WorkerId> = all.iter().collect();
    let choice = Scorer::default().choose_among(&fake, &refs).unwrap();
    assert_eq!(choice.worker, &worker("b"));
    assert!(choice.scored && !choice.concentrated && !choice.distrusted && !choice.slack_used);
    assert!(!choice.tie_break);

    // Identical hosts: the tie-break decides.
    let (x, y) = twins();
    let fake = Fake::new(vec![("x", x), ("y", y)]);
    let all = names(&["x", "y"]);
    let refs: Vec<&WorkerId> = all.iter().collect();
    assert!(
        Scorer::default()
            .choose_among(&fake, &refs)
            .unwrap()
            .tie_break
    );

    // One candidate: taken, not scored.
    let one = Scorer::default().choose_among(&fake, &refs[..1]).unwrap();
    assert!(!one.scored);
}

mod explained;
