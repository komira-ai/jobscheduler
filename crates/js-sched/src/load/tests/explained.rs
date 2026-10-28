//! Fit shapes and the rule that decided a choice.

use super::*;

fn with_fit(fake: &Fake, fit: FitShape, name: &str) -> ScoreKey {
    let config = ScoreConfig::default().with_fit(fit);
    Scorer::new(config).key(fake, &worker(name))
}

fn pick_with(fake: &Fake, fit: FitShape, order: &[&str]) -> (String, DecidedBy) {
    let candidates = names(order);
    let config = ScoreConfig::default().with_fit(fit);
    let (w, by) = Scorer::new(config)
        .choose_explained(fake, &candidates)
        .expect("a candidate");
    (w.as_str().to_owned(), by)
}

/// Two hosts of 1000 millicores and 1000 bytes, one minute each, nothing asked for: `a`
/// has CPU and memory leftovers of 20% and 20%; `b` of 15% and 30%.
fn square_versus_line() -> Fake {
    let a = Host::empty(1_000, 1_000)
        .cpu_load(at_slot(0, 800))
        .mem_load(at_slot(0, 800));
    let b = Host::empty(1_000, 1_000)
        .cpu_load(at_slot(0, 850))
        .mem_load(at_slot(0, 700));
    Fake::new(vec![("a", a), ("b", b)])
}

#[test]
fn the_squared_and_the_linear_fit_choose_differently_on_a_fixed_case() {
    let fake = square_versus_line();
    // Squared: a 2 * 200k^2 = 80e9; b 2 * 150k^2 + 150k^2 = 67.5e9, b is the tighter fit.
    // Linear: a 2 * 200k = 400k; b 2 * 150k + 150k = 450k, a is the tighter fit.
    assert_eq!(
        pick_with(&fake, FitShape::Squared, &["a", "b"]),
        ("b".to_owned(), DecidedBy::Key(5))
    );
    assert_eq!(
        pick_with(&fake, FitShape::Linear, &["a", "b"]),
        ("a".to_owned(), DecidedBy::Key(5))
    );
    assert_eq!(pick(&fake, &["a", "b"]), "b", "squared is the default");
    let fit = |shape, name| with_fit(&fake, shape, name).fit();
    assert_eq!(fit(FitShape::Squared, "a"), 31 * 80_000_000_000);
    assert_eq!(fit(FitShape::Linear, "a"), 31 * 400_000);
    assert_eq!(fit(FitShape::Linear, "b"), 31 * 450_000);
}

#[test]
fn the_fit_shape_changes_only_key_5() {
    let mut hosts = vec![(
        "calm",
        Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 20_000),
    )];
    let mut busy = Host::empty(100_000, BIG_MEMORY)
        .cpu_load(|s| if s < 12 { 70_000 } else { 30_000 })
        .cpu_new(|_| 20_000)
        .mem_load(|_| BIG_MEMORY - 5)
        .mem_new(|_| 4)
        .life(12);
    busy.facts.fallback = true;
    busy.facts.same_key_millis = 40_000;
    busy.facts.uncovered_leases = 2;
    hosts.push(("busy", busy));
    let fake = Fake::new(hosts);
    for name in ["calm", "busy"] {
        let base = with_fit(&fake, FitShape::Squared, name);
        for shape in [FitShape::Linear, FitShape::Flat] {
            let other = with_fit(&fake, shape, name);
            assert_eq!(
                (base.slack(), base.memory_unsafe(), base.distrusted()),
                (other.slack(), other.memory_unsafe(), other.distrusted())
            );
            assert_eq!(base.concentrated(), other.concentrated());
            assert_eq!(
                (base.uncovered(), base.cpu_capacity_millis(), base.hash()),
                (other.uncovered(), other.cpu_capacity_millis(), other.hash())
            );
            assert_eq!(base.near_headroom_millis(), other.near_headroom_millis());
        }
    }
    assert!(with_fit(&fake, FitShape::Squared, "busy").distrusted());
    assert!(with_fit(&fake, FitShape::Squared, "busy").memory_unsafe());
    assert_ne!(
        with_fit(&fake, FitShape::Squared, "busy").fit(),
        with_fit(&fake, FitShape::Linear, "busy").fit()
    );
    // The same evaluations in every shape.
    let mut counts = Vec::new();
    for shape in [FitShape::Squared, FitShape::Linear, FitShape::Flat] {
        let mut scorer = Scorer::new(ScoreConfig::default().with_fit(shape));
        scorer.choose(&fake, &names(&["calm", "busy"]));
        counts.push(scorer.evaluations());
    }
    assert!(counts.iter().all(|c| *c == counts[0]));
}

#[test]
fn the_flat_fit_charges_the_mean_over_the_lease_s_minutes_to_every_slot() {
    // Two minutes of life, CPU only. `front` carries 900 then 100 of 1000, `even` 500 and
    // 500: the same mean. Squared tells them apart; Flat does not, and a tie-break decides.
    let host = |first: u64, second: u64| {
        Host::empty(1_000, BIG_MEMORY)
            .cpu_load(move |s| match s {
                0 => first,
                1 => second,
                _ => 0,
            })
            .life(2)
    };
    let fake = Fake::new(vec![("front", host(900, 100)), ("even", host(500, 500))]);
    let (_, by) = pick_with(&fake, FitShape::Squared, &["front", "even"]);
    assert_eq!(by, DecidedBy::Key(5));
    let fit = |shape, name| with_fit(&fake, shape, name).fit();
    assert_ne!(
        fit(FitShape::Squared, "front"),
        fit(FitShape::Squared, "even")
    );
    assert_eq!(fit(FitShape::Flat, "front"), fit(FitShape::Flat, "even"));
    let (_, by) = pick_with(&fake, FitShape::Flat, &["front", "even"]);
    assert!(matches!(by, DecidedBy::TieBreak(_)), "{by:?}");
    // By hand: the mean leaves 50% of CPU and all of the memory in both slots, weights 31
    // and 30: 61 * (2 * 500k^2 + 500k^2).
    assert_eq!(
        fit(FitShape::Flat, "even"),
        61 * (2 * 500_000u128 * 500_000 + 500_000 * 500_000)
    );

    // The new lease's charge is flattened too: all in the first minute against spread.
    let host = |first: u64, second: u64| {
        Host::empty(1_000, BIG_MEMORY)
            .cpu_new(move |s| match s {
                0 => first,
                1 => second,
                _ => 0,
            })
            .life(2)
    };
    let fake = Fake::new(vec![("front", host(800, 0)), ("even", host(400, 400))]);
    let fit = |shape, name| with_fit(&fake, shape, name).fit();
    assert_ne!(
        fit(FitShape::Squared, "front"),
        fit(FitShape::Squared, "even")
    );
    assert_eq!(fit(FitShape::Flat, "front"), fit(FitShape::Flat, "even"));
}

#[test]
fn a_flat_charged_lease_fits_the_same_in_every_shape() {
    // With one minute of life the mean over it is the value itself.
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..60 {
        let mut host = random_host(&mut rng);
        host.facts.fit_minutes = 1;
        let fake = Fake::new(vec![("h", host)]);
        let squared = with_fit(&fake, FitShape::Squared, "h").fit();
        assert_eq!(
            with_fit(&fake, FitShape::Flat, "h").fit(),
            squared,
            "{round}"
        );
    }
}

/// Twin hosts with the first four keys equal, for a case decided by one key.
fn decided(fake: &Fake) -> DecidedBy {
    pick_with(fake, FitShape::Squared, &["a", "b"]).1
}

#[test]
fn choose_explained_names_the_key_that_decided() {
    let base = || Host::empty(100_000, 1_000_000).cpu_load(|_| 20_000);
    let one = |a: Host, b: Host| Fake::new(vec![("a", a), ("b", b)]);

    // Key 1: `a` is over 100% at every slot.
    let mut a = base();
    a = a.cpu_load(|_| 120_000);
    assert_eq!(decided(&one(a, base())), DecidedBy::Key(1));

    // Key 2: 700 + 1.3 * 300 is over 1000 but 700 + 300 is not.
    let mem = |load: u64| {
        Host::empty(100_000, 1_000)
            .mem_load(move |_| load)
            .mem_new(|_| 300)
    };
    let fake = one(mem(700), mem(100));
    assert_eq!(decided(&fake), DecidedBy::Key(2));
    assert_eq!(pick_with(&fake, FitShape::Squared, &["a", "b"]).0, "b");

    // Key 3: `a` is distrusted.
    let mut a = base();
    a.facts.fallback = true;
    assert_eq!(decided(&one(a, base())), DecidedBy::Key(3));
    let mut a = base();
    a.facts.measured_above_predicted = true;
    assert_eq!(decided(&one(a, base())), DecidedBy::Key(3));

    // Key 4: `a` is over its share of one key.
    let mut a = base();
    a.facts.same_key_millis = 30_000;
    assert_eq!(decided(&one(a, base())), DecidedBy::Key(4));

    // Key 5: the fit.
    let a = base().cpu_load(|_| 30_000);
    let fake = one(a, base());
    assert_eq!(decided(&fake), DecidedBy::Key(5));
    assert_eq!(pick_with(&fake, FitShape::Squared, &["a", "b"]).0, "a");
}

#[test]
fn choose_explained_names_the_tie_break_that_decided() {
    let twin = || Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 20_000);

    // Tie-break 1: fewer uncovered leases.
    let mut a = twin();
    a.facts.uncovered_leases = 1;
    let fake = Fake::new(vec![("a", a), ("b", twin())]);
    assert_eq!(
        pick_with(&fake, FitShape::Squared, &["a", "b"]),
        ("b".to_owned(), DecidedBy::TieBreak(1))
    );

    // Tie-break 2: the larger host, the same fractions.
    let large = Host::empty(200_000, BIG_MEMORY)
        .cpu_load(|_| 40_000)
        .cpu_new(|_| 20_000);
    let small = Host::empty(100_000, BIG_MEMORY)
        .cpu_load(|_| 20_000)
        .cpu_new(|_| 10_000);
    let fake = Fake::new(vec![("a", small), ("b", large)]);
    assert_eq!(
        pick_with(&fake, FitShape::Squared, &["a", "b"]),
        ("b".to_owned(), DecidedBy::TieBreak(2))
    );

    // Tie-break 3: the headroom over the next five minutes.
    let fake = Fake::new(vec![
        (
            "a",
            twin().cpu_load(|s| if s == 3 { 30_000 } else { 20_000 }),
        ),
        ("b", twin()),
    ]);
    assert_eq!(
        pick_with(&fake, FitShape::Squared, &["a", "b"]),
        ("b".to_owned(), DecidedBy::TieBreak(3))
    );

    // Tie-break 4: the hash.
    let fake = Fake::new(vec![("a", twin()), ("b", twin())]);
    let want = by_hash(&fake, "a", "b");
    assert_eq!(
        pick_with(&fake, FitShape::Squared, &["a", "b"]),
        (want, DecidedBy::TieBreak(4))
    );

    // Tie-break 5: two keys equal in everything but the worker id (a hash collision
    // between two workers cannot be built, so the key is edited).
    let k = key(&fake, "a");
    let mut other = k.clone();
    other.worker = worker("zz");
    assert_eq!(k.decided_by(&other), DecidedBy::TieBreak(5));
    assert_eq!(k.worker(), &worker("a"));
}

#[test]
fn choose_explained_says_only_for_one_candidate_and_none_for_none() {
    let (a, b) = twins();
    let fake = Fake::new(vec![("a", a), ("b", b)]);
    let mut scorer = Scorer::default();
    let one = names(&["b"]);
    assert_eq!(
        scorer.choose_explained(&fake, &one),
        Some((&one[0], DecidedBy::Only))
    );
    assert_eq!(scorer.choose_explained(&fake, &[]), None);
    assert_eq!(scorer.evaluations(), 0);
}

#[test]
fn choose_and_choose_explained_choose_the_same_worker() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut kinds = std::collections::BTreeSet::new();
    for round in 0..300 {
        let count = 1 + rng.below(6) as usize;
        let all: Vec<(String, Host)> = (0..count)
            .map(|i| (format!("w{i}"), random_host(&mut rng)))
            .collect();
        let mut fake = Fake::new(all.iter().map(|(n, h)| (n.as_str(), h.clone())).collect());
        fake.lease.lease = LeaseId::new(1, round);
        let candidates: Vec<WorkerId> = all.iter().map(|(n, _)| worker(n)).collect();
        for fit in [FitShape::Squared, FitShape::Linear, FitShape::Flat] {
            let config = ScoreConfig::default().with_fit(fit);
            let chosen = Scorer::new(config).choose(&fake, &candidates);
            let (explained, by) = Scorer::new(config)
                .choose_explained(&fake, &candidates)
                .unzip();
            assert_eq!(chosen, explained, "round {round} {fit:?}");
            // The rule is the first difference from the best of the others, computed
            // here from the sorted keys and not from the scorer's own ranking.
            if count > 1 {
                let mut scorer = Scorer::new(config);
                let mut keys: Vec<ScoreKey> =
                    candidates.iter().map(|w| scorer.key(&fake, w)).collect();
                keys.sort();
                assert_eq!(
                    by,
                    Some(keys[0].decided_by(&keys[1])),
                    "round {round} {fit:?}"
                );
            }
            kinds.extend(by);
        }
    }
    let keys = |n| kinds.contains(&DecidedBy::Key(n));
    assert!(kinds.contains(&DecidedBy::Only));
    assert!(keys(1) && keys(5), "{kinds:?}");
}

#[test]
fn choose_explained_compares_the_winner_with_the_best_of_the_others() {
    let twin = || Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 20_000);
    // `a` is best; `b` is far worse on key 1; `c` differs from `a` only at tie-break 3.
    // The runner-up is `c`, so the answer is tie-break 3 whatever the order, including
    // orders where the best changes part-way and where the worst comes before the rest.
    let fake = Fake::new(vec![
        ("a", twin()),
        ("b", twin().cpu_load(|_| 120_000)),
        (
            "c",
            twin().cpu_load(|s| if s == 3 { 30_000 } else { 20_000 }),
        ),
    ]);
    let orders = [
        ["a", "b", "c"],
        ["a", "c", "b"],
        ["b", "a", "c"],
        ["b", "c", "a"],
        ["c", "a", "b"],
        ["c", "b", "a"],
    ];
    for order in orders {
        assert_eq!(
            pick_with(&fake, FitShape::Squared, &order),
            ("a".to_owned(), DecidedBy::TieBreak(3)),
            "{order:?}"
        );
    }
}
