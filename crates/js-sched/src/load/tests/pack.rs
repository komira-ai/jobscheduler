//! The pack limit: the rule between key 2 and key 3 of the score.

use super::*;

const CPU: u64 = 100_000;
const MEMORY: u64 = 1_000_000;

/// A worker of `cpu_capacity` millicores and `memory_capacity` bytes that has `cpu`
/// millicores and `memory` bytes booked with the new lease. Its predicted load is nothing,
/// so its fit is the same as another such worker's until `cpu_load` sets it.
fn sized(cpu_capacity: u64, memory_capacity: u64, cpu: u64, memory: u64) -> Host {
    let mut host = Host::empty(cpu_capacity, memory_capacity);
    host.facts.booked_with_lease = Charge {
        cpu_millis: cpu,
        memory_bytes: memory,
    };
    host
}

/// A worker of [`CPU`] millicores and [`MEMORY`] bytes with `cpu` and `memory` booked.
fn booked(cpu: u64, memory: u64) -> Host {
    sized(CPU, MEMORY, cpu, memory)
}

fn pack_of(host: Host) -> Pack {
    key(&Fake::new(vec![("a", host)]), "a").pack()
}

fn explained(fake: &Fake, order: &[&str]) -> (String, DecidedBy) {
    let candidates = names(order);
    let (w, by) = Scorer::default()
        .choose_explained(fake, &candidates)
        .expect("a candidate");
    (w.as_str().to_owned(), by)
}

/// Behaviour: a worker whose bookings with the lease are exactly the limit (0.8x of its
/// CPU, 800 permille) is within it; one millicore more is past it, with the share in parts
/// per million rounded up.
/// Catches: the limit compared with `<` instead of `<=`.
#[test]
fn exactly_at_the_limit_is_within_and_one_millicore_over_is_past() {
    assert_eq!(pack_of(booked(80_000, 0)), Pack::Within);
    assert_eq!(pack_of(booked(80_001, 0)), Pack::Past(800_010));
    assert_eq!(pack_of(booked(0, 0)), Pack::Within);
}

/// Behaviour: memory is the binding resource when it is the larger share: low CPU with
/// memory over the limit is past, with the memory's share; memory exactly at the limit is
/// within; when the CPU share is the larger it binds instead.
#[test]
fn memory_is_the_binding_share_when_it_is_the_larger() {
    assert_eq!(pack_of(booked(10_000, 800_000)), Pack::Within);
    assert_eq!(pack_of(booked(10_000, 900_000)), Pack::Past(900_000));
    assert_eq!(pack_of(booked(95_000, 850_000)), Pack::Past(950_000));
}

/// Behaviour: a resource the worker has none of is one it cannot fill: its share is 0, so
/// the other resource alone decides, and a worker with neither is within.
#[test]
fn a_resource_of_capacity_zero_has_share_zero() {
    assert_eq!(pack_of(sized(0, MEMORY, 5_000, 500_000)), Pack::Within);
    assert_eq!(
        pack_of(sized(0, MEMORY, 5_000, 900_000)),
        Pack::Past(900_000)
    );
    assert_eq!(pack_of(sized(CPU, 0, 50_000, 7_000)), Pack::Within);
    assert_eq!(pack_of(sized(CPU, 0, 90_000, 7_000)), Pack::Past(900_000));
    assert_eq!(pack_of(sized(0, 0, 5_000, 7_000)), Pack::Within);
}

/// Behaviour: the share is rounded up to the next part per million, so a worker a hair past
/// the limit is never taken for one within it (1 of 3 millicores is 333333.3 ppm: past a
/// limit of 0, as 333334).
#[test]
fn the_share_is_rounded_up() {
    let fake = Fake::new(vec![("a", sized(3, MEMORY, 1, 0))]);
    let config = ScoreConfig::default().with_pack_limit(0);
    let key = Scorer::new(config).key(&fake, &worker("a"));
    assert_eq!(key.pack(), Pack::Past(333_334));
}

/// Behaviour: among workers past the limit the one with the smaller booked share comes
/// first, although the fit (key 5) prefers the fullest: of 90%, 95% and 85% the 85% is
/// taken, in whatever order they are offered.
/// Catches: past-limit workers packed by the fit, the way within-limit workers are.
#[test]
fn past_the_limit_the_least_full_host_comes_first() {
    // The fuller the predicted load (CPU and memory alike), the better the fit.
    let fake = Fake::new(vec![
        (
            "p90",
            booked(90_000, 0).cpu_load(|_| 80_000).mem_load(|_| 800_000),
        ),
        (
            "p95",
            booked(95_000, 0).cpu_load(|_| 90_000).mem_load(|_| 900_000),
        ),
        (
            "p85",
            booked(85_000, 0).cpu_load(|_| 70_000).mem_load(|_| 700_000),
        ),
    ]);
    assert!(key(&fake, "p95").fit() < key(&fake, "p90").fit());
    assert!(key(&fake, "p90").fit() < key(&fake, "p85").fit());
    for order in [
        ["p90", "p95", "p85"],
        ["p95", "p85", "p90"],
        ["p85", "p90", "p95"],
    ] {
        assert_eq!(pick(&fake, &order), "p85", "order {order:?}");
    }
    assert_eq!(pick(&fake, &["p95", "p90"]), "p90");
}

/// Behaviour: a worker within the limit comes before one past it even when the past one is
/// the better fit.
/// Catches: the pack limit placed after key 5.
#[test]
fn within_the_limit_beats_past_it_even_when_the_fit_is_worse() {
    let fake = Fake::new(vec![
        ("within", booked(70_000, 0).cpu_load(|_| 10_000)),
        ("past", booked(85_000, 0).cpu_load(|_| 84_000)),
    ]);
    assert!(key(&fake, "past").fit() < key(&fake, "within").fit());
    assert_eq!(pick(&fake, &["past", "within"]), "within");
    assert_eq!(pick(&fake, &["within", "past"]), "within");
}

/// Behaviour: within the limit the fit still decides: of two workers within it, the fuller
/// (the tighter fit) is taken, and the deciding rule is key 5.
#[test]
fn within_the_limit_the_fullest_host_is_still_packed() {
    let fake = Fake::new(vec![
        ("full", booked(70_000, 0).cpu_load(|_| 60_000)),
        ("light", booked(30_000, 0).cpu_load(|_| 20_000)),
    ]);
    assert_eq!(pick(&fake, &["light", "full"]), "full");
    assert_eq!(
        explained(&fake, &["light", "full"]),
        ("full".to_owned(), DecidedBy::Key(5))
    );
}

/// Behaviour: key 1 (overshoot of the capacity) is above the pack limit: a worker within
/// the limit in bookings but over its capacity in predicted load loses to a worker past
/// the limit with no overshoot.
#[test]
fn key_1_comes_before_the_pack_limit() {
    let fake = Fake::new(vec![
        ("slack", booked(10_000, 0).cpu_load(|_| 120_000)),
        ("none", booked(95_000, 0)),
    ]);
    assert!(key(&fake, "slack").slack() > 0);
    assert_eq!(key(&fake, "none").slack(), 0);
    assert_eq!(key(&fake, "slack").pack(), Pack::Within);
    assert!(matches!(key(&fake, "none").pack(), Pack::Past(_)));
    assert_eq!(
        explained(&fake, &["slack", "none"]),
        ("none".to_owned(), DecidedBy::Key(1))
    );
}

/// Behaviour: key 2 (no room for a memory miss) is above the pack limit: a worker within
/// the limit that has no room for the memory miss loses to a worker past the limit that
/// has.
#[test]
fn key_2_comes_before_the_pack_limit() {
    let fake = Fake::new(vec![
        // 900k of 1M asked: with the 1.3 miss it does not fit.
        ("unsafe", booked(10_000, 0).mem_new(|_| 900_000)),
        ("safe", booked(95_000, 0).mem_new(|_| 100_000)),
    ]);
    assert!(key(&fake, "unsafe").memory_unsafe());
    assert!(!key(&fake, "safe").memory_unsafe());
    assert_eq!(key(&fake, "unsafe").slack(), 0);
    assert_eq!(key(&fake, "safe").slack(), 0);
    assert_eq!(key(&fake, "unsafe").pack(), Pack::Within);
    assert_eq!(
        explained(&fake, &["unsafe", "safe"]),
        ("safe".to_owned(), DecidedBy::Key(2))
    );
}

/// Behaviour: the pack limit is above key 3: a distrusted worker (in fallback) within the
/// limit beats a trusted worker past it.
/// Catches: the pack limit placed after key 3.
#[test]
fn the_pack_limit_comes_before_key_3() {
    let mut distrusted = booked(50_000, 0);
    distrusted.facts.fallback = true;
    let fake = Fake::new(vec![
        ("distrusted", distrusted),
        ("trusted", booked(90_000, 0)),
    ]);
    assert!(key(&fake, "distrusted").distrusted());
    assert!(!key(&fake, "trusted").distrusted());
    assert_eq!(pick(&fake, &["trusted", "distrusted"]), "distrusted");
    assert_eq!(
        explained(&fake, &["trusted", "distrusted"]),
        ("distrusted".to_owned(), DecidedBy::PackLimit)
    );
}

/// Behaviour: the rule that decided is the pack limit when one worker is within and the
/// other past it, and when both are past it with different shares.
#[test]
fn the_pack_limit_is_reported_as_the_deciding_rule() {
    let fake = Fake::new(vec![
        ("within", booked(70_000, 0)),
        ("past_a", booked(85_000, 0)),
        ("past_b", booked(90_000, 0)),
    ]);
    assert_eq!(
        explained(&fake, &["past_a", "within"]),
        ("within".to_owned(), DecidedBy::PackLimit)
    );
    assert_eq!(
        explained(&fake, &["past_b", "past_a"]),
        ("past_a".to_owned(), DecidedBy::PackLimit)
    );
    let a = key(&fake, "past_b");
    let b = key(&fake, "past_a");
    assert_eq!(a.decided_by(&b), DecidedBy::PackLimit);
    assert_eq!(b.decided_by(&a), DecidedBy::PackLimit);
}

/// Behaviour: a choice between two workers past the limit with the same share is made by a
/// tie-break and reported as one; with different shares it is not.
#[test]
fn equal_shares_past_the_limit_are_a_tie_break() {
    let tie = Fake::new(vec![("a", booked(90_000, 0)), ("b", booked(90_000, 0))]);
    let candidates = [worker("a"), worker("b")];
    let refs: Vec<&WorkerId> = candidates.iter().collect();
    let choice = Scorer::default()
        .choose_among(&tie, &refs)
        .expect("a candidate");
    assert!(choice.scored);
    assert!(choice.tie_break);
    let (_, by) = explained(&tie, &["a", "b"]);
    assert!(matches!(by, DecidedBy::TieBreak(_)), "{by:?}");

    let apart = Fake::new(vec![("a", booked(90_000, 0)), ("b", booked(91_000, 0))]);
    let choice = Scorer::default()
        .choose_among(&apart, &refs)
        .expect("a candidate");
    assert_eq!(choice.worker.as_str(), "a");
    assert!(!choice.tie_break);
}

/// Behaviour: `with_pack_limit` sets the limit, brings one above 1.0x down to it, and
/// `pack_limit_permille` reads it; the limit decides where a worker stands: 60% booked is
/// within a limit of 600 and past one of 599, and the whole capacity is within 1000.
#[test]
fn with_pack_limit_sets_and_clamps() {
    let config = ScoreConfig::default();
    assert_eq!(config.with_pack_limit(500).pack_limit_permille(), 500);
    assert_eq!(config.with_pack_limit(0).pack_limit_permille(), 0);
    assert_eq!(config.with_pack_limit(1000).pack_limit_permille(), 1000);
    assert_eq!(config.with_pack_limit(1001).pack_limit_permille(), 1000);
    assert_eq!(config.with_pack_limit(u32::MAX).pack_limit_permille(), 1000);

    let fake = Fake::new(vec![
        ("sixty", booked(60_000, 0)),
        ("whole", booked(100_000, MEMORY)),
        ("over", booked(100_001, 0)),
    ]);
    let at = |limit: u32, name: &str| {
        Scorer::new(ScoreConfig::default().with_pack_limit(limit))
            .key(&fake, &worker(name))
            .pack()
    };
    assert_eq!(at(600, "sixty"), Pack::Within);
    assert_eq!(at(599, "sixty"), Pack::Past(600_000));
    assert_eq!(at(1000, "whole"), Pack::Within);
    assert_eq!(at(1000, "over"), Pack::Past(1_000_010));
    assert_eq!(at(5_000, "whole"), Pack::Within);
    assert_eq!(at(5_000, "over"), Pack::Past(1_000_010));
}

/// Behaviour: a config made by `ScoreConfig::new` and the default one both have the
/// default pack limit, 800 permille (0.8x).
#[test]
fn the_default_pack_limit_is_800_permille() {
    assert_eq!(DEFAULT_PACK_LIMIT_PERMILLE, 800);
    assert_eq!(ScoreConfig::default().pack_limit_permille(), 800);
    let made = ScoreConfig::new(1, 10_000, 250_000).expect("balance in range");
    assert_eq!(made.pack_limit_permille(), 800);
    assert_eq!(made, ScoreConfig::default());
}

/// Behaviour: `ScoreConfig::of` carries the placement setting `pack_limit_permille` and
/// brings one above 1000 down to it.
#[test]
fn the_score_config_of_a_load_config_carries_the_pack_limit() {
    let of = |permille| {
        ScoreConfig::of(&crate::LoadConfig {
            pack_limit_permille: permille,
            ..crate::LoadConfig::default()
        })
    };
    assert_eq!(
        ScoreConfig::of(&crate::LoadConfig::default()).pack_limit_permille(),
        800
    );
    assert_eq!(of(650).pack_limit_permille(), 650);
    assert_eq!(of(0).pack_limit_permille(), 0);
    assert_eq!(of(5_000).pack_limit_permille(), 1000);
}

/// Behaviour: the pack comparison orders within before every past, and past by share.
#[test]
fn within_orders_before_past_and_past_by_share() {
    assert!(Pack::Within < Pack::Past(0));
    assert!(Pack::Past(800_010) < Pack::Past(900_000));
}

/// Behaviour: a share too large for parts per million in 64 bits (a worker of one
/// millicore booked to the largest count) is capped at the largest value, still past the
/// limit and after every smaller share.
#[test]
fn a_share_beyond_64_bits_is_capped() {
    assert_eq!(pack_of(sized(1, MEMORY, u64::MAX, 0)), Pack::Past(u64::MAX));
    assert!(Pack::Past(900_000) < Pack::Past(u64::MAX));
}
