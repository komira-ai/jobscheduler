//! The spread fit ([`FitShape::Spread`]): the emptiest host first, by the share of its binding
//! resource in use once the lease is on it, and key 3 for a host in fallback only.

use super::*;

fn spread() -> Scorer {
    Scorer::new(ScoreConfig::default().with_fit(FitShape::Spread))
}

fn spread_key(fake: &Fake, name: &str) -> ScoreKey {
    spread().key(fake, &worker(name))
}

fn spread_pick(fake: &Fake, order: &[&str]) -> (String, DecidedBy) {
    let candidates = names(order);
    let (w, by) = spread()
        .choose_explained(fake, &candidates)
        .expect("a candidate");
    (w.as_str().to_owned(), by)
}

/// Catches: the spread fit ranking the fuller host first, as the squared fit does. `full`
/// carries 80 of 100 cores and `light` 20; a lease of 10 cores fits both inside 100%. The
/// spread fit sends it to `light`, by key 5, in either order of the candidates; the squared
/// fit sends it to `full`.
#[test]
fn the_spread_fit_takes_the_emptiest_host_and_the_squared_fit_the_fullest() {
    let host = |load: u64| {
        Host::empty(100_000, BIG_MEMORY)
            .cpu_load(move |_| load)
            .cpu_new(|_| 10_000)
    };
    let fake = Fake::new(vec![("full", host(80_000)), ("light", host(20_000))]);
    for order in [["full", "light"], ["light", "full"]] {
        assert_eq!(
            spread_pick(&fake, &order),
            ("light".to_owned(), DecidedBy::Key(5))
        );
        assert_eq!(pick(&fake, &order), "full");
    }
    // By hand: one fit minute, weight 31; CPU binds (memory is all free). `light` is left
    // 70% (30% in use), `full` 10% (90% in use).
    assert_eq!(spread_key(&fake, "light").fit(), 31 * 300_000);
    assert_eq!(spread_key(&fake, "full").fit(), 31 * 900_000);
}

/// Catches: the spread fit reading the CPU alone. `cpu_free` is left 60% of its CPU but 10%
/// of its memory, `even` 40% of each: memory binds on `cpu_free`, 90% in use, so `even` (60%
/// in use) is the emptier and takes the lease.
#[test]
fn the_spread_fit_reads_the_binding_resource() {
    let cpu_free = Host::empty(1_000, 1_000)
        .cpu_load(|_| 400)
        .mem_load(|_| 900);
    let even = Host::empty(1_000, 1_000)
        .cpu_load(|_| 600)
        .mem_load(|_| 600);
    let fake = Fake::new(vec![("cpu_free", cpu_free), ("even", even)]);
    assert_eq!(
        spread_pick(&fake, &["cpu_free", "even"]),
        ("even".to_owned(), DecidedBy::Key(5))
    );
    assert_eq!(spread_key(&fake, "cpu_free").fit(), 31 * 900_000);
    assert_eq!(spread_key(&fake, "even").fit(), 31 * 600_000);
}

/// Catches: the spread fit reading the first minute alone, or the minutes after the lease's
/// life. With two minutes of life, `front` is 90% in use in minute 0 and idle in minute 1,
/// `even` 50% in both, and both are 100% in use from minute 2 on: the fit weighs minute 0 at
/// 31 and minute 1 at 30 and nothing after, so `front` (27.9 million) is before `even` (30.5
/// million); on minute 0 alone `even` would be.
#[test]
fn the_spread_fit_weighs_each_minute_of_the_lease_s_life() {
    let host = |first: u64, second: u64| {
        Host::empty(1_000, BIG_MEMORY)
            .cpu_load(move |s| match s {
                0 => first,
                1 => second,
                _ => 1_000,
            })
            .life(2)
    };
    let fake = Fake::new(vec![("front", host(900, 0)), ("even", host(500, 500))]);
    assert_eq!(spread_key(&fake, "front").fit(), 31 * 900_000);
    assert_eq!(spread_key(&fake, "even").fit(), 61 * 500_000);
    assert_eq!(
        spread_pick(&fake, &["even", "front"]),
        ("front".to_owned(), DecidedBy::Key(5))
    );
}

/// Catches: under the spread fit, a host measured above the predictions of its leases still
/// distrusted (an idle host whose own services keep a core busy then loses every lease to a
/// busy host), and a host in fallback no longer distrusted. `above` and `fallback` carry 1 of
/// 100 cores, `busy` 70.
#[test]
fn under_the_spread_fit_only_a_host_in_fallback_is_distrusted() {
    let idle = || Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 1_000);
    let mut above = idle();
    above.facts.measured_above_predicted = true;
    let mut fallback = idle();
    fallback.facts.fallback = true;
    let busy = Host::empty(100_000, BIG_MEMORY).cpu_load(|_| 70_000);
    let fake = Fake::new(vec![
        ("above", above),
        ("fallback", fallback),
        ("busy", busy),
    ]);
    assert!(!spread_key(&fake, "above").distrusted());
    assert!(spread_key(&fake, "fallback").distrusted());
    assert!(!spread_key(&fake, "busy").distrusted());
    assert!(
        key(&fake, "above").distrusted(),
        "the squared fit distrusts it"
    );

    assert_eq!(
        spread_pick(&fake, &["busy", "above"]),
        ("above".to_owned(), DecidedBy::Key(5))
    );
    assert_eq!(pick(&fake, &["busy", "above"]), "busy");
    // A host in fallback still loses on key 3, however empty it looks.
    assert_eq!(
        spread_pick(&fake, &["fallback", "busy"]),
        ("busy".to_owned(), DecidedBy::Key(3))
    );
}

/// Catches: the spread fit going wrong at the edge of the capacity. A lease that fills a host
/// exactly leaves nothing: the whole capacity is in use (31 x 10^6), with no slack; one that
/// goes 5% past it is in use the same, and loses to a host with room on key 1.
#[test]
fn the_spread_fit_at_and_past_the_capacity() {
    let host = |load: u64| {
        Host::empty(100_000, BIG_MEMORY)
            .cpu_load(move |_| load)
            .cpu_new(|_| 10_000)
    };
    let fake = Fake::new(vec![
        ("exact", host(90_000)),
        ("over", host(95_000)),
        ("room", host(50_000)),
    ]);
    let exact = spread_key(&fake, "exact");
    assert_eq!((exact.slack(), exact.fit()), (0, 31 * 1_000_000));
    let over = spread_key(&fake, "over");
    assert!(over.slack() > 0);
    assert_eq!(over.fit(), 31 * 1_000_000);
    assert_eq!(
        spread_pick(&fake, &["over", "room"]),
        ("room".to_owned(), DecidedBy::Key(1))
    );
    assert_eq!(
        spread_pick(&fake, &["exact", "room"]),
        ("room".to_owned(), DecidedBy::Key(5))
    );
}
