//! The admission predicate stand-in: slot 0 against the flat rule's cases, the later
//! slots, the caps, and the sums kept incrementally against the sums built from scratch.

use js_sim::SimRng;

use super::flat;
use crate::engine::model::{Belief, Capacity, Charge, H_PLUS, HostState, Pool};
use crate::engine::predicate::{Params, Refusal, admit, cold_cap, left};

/// An 8-core host with 32 GiB, the one the flat rule's cases use.
fn node() -> Capacity {
    Capacity {
        cpu_millis: 8_000,
        mem_mib: 32 * 1_024,
        leases: 64,
    }
}

/// Window core-ms of a host that was busy at `millis` millicores for 5 s.
fn core_ms(millis: u64) -> u64 {
    millis * 5
}

/// Places `n` leases believed to be `b` at `tick`, as one round.
fn place_round<'w>(host: &mut HostState<'w>, first: usize, n: usize, tick: u64, b: &'w Belief) {
    host.begin_round(tick);
    for i in 0..n {
        host.place(first + i, tick, b);
    }
}

/// Catches: a slot 0 that is not the flat rule's. The cases are the flat rule's own
/// (restated; the 8-core node of its tests): the uncovered charge in full, the landing
/// exactly on `C (1 + s_cpu)` admitted and a milli over refused, the floor `f` of the
/// covered under a low reading rounded up, a rise believed at once and a fall after two
/// windows, leases a reading has not covered carrying their full cost, predicted memory
/// held whatever the working set says, and the working set counted when it is more.
#[test]
fn slot_zero_follows_the_flat_rule() {
    let p = Params::default();
    let half = flat(500, 0);

    // 17 leases of 500 m placed in one round are uncovered: 8500 of 8800.
    let mut h = HostState::new(node());
    place_round(&mut h, 0, 17, 0, &half);
    assert_eq!(left(&p, &h).cpu_millis, 8_500);
    assert_eq!(admit(&p, &h, &flat(300, 0)), Ok(()), "exactly on the limit");
    assert_eq!(
        admit(&p, &h, &flat(301, 0)),
        Err(Refusal::Cpu { slot: 0 }),
        "one milli over"
    );

    // Eight covered leases (4000) with 1000 m measured are charged f of 4000 = 3000.
    let mut h = HostState::new(node());
    place_round(&mut h, 0, 8, 0, &half);
    h.begin_round(5);
    h.read(core_ms(1_000), 0);
    h.read(core_ms(1_000), 0);
    assert_eq!(left(&p, &h).cpu_millis, 3_000);
    // Slot 0 admits 5800 (3000 + 5800 = 8800); the later slots do not, because the
    // curve rule charges the residents their full prediction ahead (4000 + 5800).
    assert_eq!(
        admit(&p, &h, &flat(5_800, 0)),
        Err(Refusal::Cpu { slot: 1 })
    );
    assert_eq!(admit(&p, &h, &flat(4_800, 0)), Ok(()));
    assert_eq!(
        admit(&p, &h, &flat(5_801, 0)),
        Err(Refusal::Cpu { slot: 0 })
    );

    // The floor is rounded up: three covered leases of 250 m, 750 m, are charged 563.
    let quarter = flat(250, 0);
    let mut h = HostState::new(node());
    place_round(&mut h, 0, 3, 0, &quarter);
    h.begin_round(5);
    h.read(0, 0);
    h.read(0, 0);
    assert_eq!(left(&p, &h).cpu_millis, 563);

    // A rise is believed at once and a fall after two windows.
    let mut h = HostState::new(node());
    place_round(&mut h, 0, 8, 0, &half);
    h.begin_round(5);
    h.read(core_ms(1_000), 0);
    h.read(core_ms(6_000), 0);
    assert_eq!(left(&p, &h).cpu_millis, 6_000);
    h.read(core_ms(1_000), 0);
    assert_eq!(left(&p, &h).cpu_millis, 6_000, "the larger of the last two");
    h.read(core_ms(1_000), 0);
    assert_eq!(left(&p, &h).cpu_millis, 3_000, "back at the floor");

    // Leases placed since the last reading carry their full cost on top of it.
    h.read(core_ms(6_000), 0);
    h.begin_round(10);
    for i in 0..4 {
        h.place(100 + i, 10, &half);
    }
    assert_eq!(left(&p, &h).cpu_millis, 6_000 + 2_000);
    h.begin_round(15);
    h.read(core_ms(6_000), 0);
    assert_eq!(
        left(&p, &h).cpu_millis,
        6_000,
        "covered, charged through the reading"
    );

    // Predicted memory is held to the end: eight leases of 4 GiB fill 32 GiB while the
    // working set is 1 GiB, and a ninth is refused on memory.
    let four = flat(250, 4_096);
    let mut h = HostState::new(node());
    place_round(&mut h, 0, 8, 0, &four);
    h.begin_round(5);
    h.read(0, 1_024);
    assert_eq!(left(&p, &h).mem_mib, 32 * 1_024);
    assert_eq!(admit(&p, &h, &four), Err(Refusal::Memory { slot: 0 }));

    // The working set counts when it is more than the predicted sum.
    let mut h = HostState::new(node());
    h.read(0, 30 * 1_024);
    assert_eq!(left(&p, &h).mem_mib, 30 * 1_024);
    assert_eq!(admit(&p, &h, &flat(0, 2_048)), Ok(()));
    assert_eq!(
        admit(&p, &h, &flat(0, 2_049)),
        Err(Refusal::Memory { slot: 0 })
    );
}

/// Catches: a horizon that stops short. A resident whose curve rises only 30 minutes on
/// refuses a lease at slot 30; one that rises at minute 31 refuses it at `H+`; the same
/// lease passes beside a flat resident; and a minute later the rise has moved a slot
/// nearer (slot 29), which is the ageing of the sums.
#[test]
fn an_overshoot_at_the_last_slots_is_refused() {
    let p = Params::default();
    let cap = Pool::ci().cap; // 16 000 m, limit 17 600
    let low = Charge::new(4_000, 100);
    let high = Charge::new(14_000, 100);
    let rises_at = |minute: usize| {
        let mut curve = vec![low; minute];
        curve.push(high);
        Belief::new(false, 30, low, curve)
    };
    let new = flat(4_000, 100);

    let at_30 = rises_at(30);
    let mut h = HostState::new(cap);
    h.place(0, 0, &at_30);
    assert_eq!(admit(&p, &h, &flat(3_000, 100)), Ok(()), "17 000 of 17 600");
    assert_eq!(admit(&p, &h, &new), Err(Refusal::Cpu { slot: 30 }));

    let at_31 = rises_at(H_PLUS);
    let mut h = HostState::new(cap);
    h.place(0, 0, &at_31);
    assert_eq!(admit(&p, &h, &new), Err(Refusal::Cpu { slot: H_PLUS }));

    let steady = flat(4_000, 100);
    let mut h = HostState::new(cap);
    h.place(0, 0, &steady);
    assert_eq!(admit(&p, &h, &new), Ok(()));

    // A minute on, the resident is one minute older: its rise is 29 minutes ahead.
    let mut h = HostState::new(cap);
    h.place(0, 0, &at_30);
    h.tick(60);
    assert_eq!(admit(&p, &h, &new), Err(Refusal::Cpu { slot: 29 }));

    // The same on memory.
    let mem_high = Charge::new(1_000, 40_000);
    let mem_low = Charge::new(1_000, 1_000);
    let mut curve = vec![mem_low; 30];
    curve.push(mem_high);
    let late_mem = Belief::new(false, 30, mem_low, curve);
    let mut h = HostState::new(cap);
    h.place(0, 0, &late_mem);
    assert_eq!(
        admit(&p, &h, &flat(1_000, 7_000)),
        Err(Refusal::Memory { slot: 30 })
    );
    assert_eq!(admit(&p, &h, &flat(1_000, 6_694)), Ok(()), "46 694 exactly");
}

/// Catches: a memory cap that is not exactly the host's memory (`s_mem` is 0), or one
/// that does not move with `s_mem`.
#[test]
fn the_memory_cap_is_the_hosts_memory() {
    let cap = Pool::ci().cap;
    assert_eq!(cap.mem_mib, 46_694);
    let mut h = HostState::new(cap);
    let resident = flat(1_000, 10_000);
    h.place(0, 0, &resident);
    let p = Params::default();
    assert_eq!(admit(&p, &h, &flat(1_000, 36_694)), Ok(()), "100% exactly");
    assert_eq!(
        admit(&p, &h, &flat(1_000, 36_695)),
        Err(Refusal::Memory { slot: 0 })
    );
    let wide = Params {
        s_mem_permille: 100,
        ..p
    };
    assert_eq!(wide.mem_limit(&cap), 51_363);
    assert_eq!(admit(&wide, &h, &flat(1_000, 41_363)), Ok(()));
    assert_eq!(
        admit(&wide, &h, &flat(1_000, 41_364)),
        Err(Refusal::Memory { slot: 0 })
    );
}

/// Catches: a cold cap that does not scale with the host (1 per window at 16 threads, 3 at
/// 88), that does not reset at the next round, that binds warm leases, or that is
/// reported when something else is the obstacle.
#[test]
fn the_cold_cap_is_per_window_and_per_host() {
    assert_eq!(cold_cap(&Pool::ci().cap), 1);
    assert_eq!(cold_cap(&Pool::full().cap), 3);
    assert_eq!(cold_cap(&Pool::new(1, 32, 1).cap), 1);
    assert_eq!(cold_cap(&Pool::new(1, 33, 1).cap), 2);
    let p = Params::default();
    let cold = Belief::cold();

    let mut h = HostState::new(Pool::ci().cap);
    h.begin_round(0);
    assert_eq!(admit(&p, &h, &cold), Ok(()));
    h.place(0, 0, &cold);
    assert_eq!(admit(&p, &h, &cold), Err(Refusal::ColdCap));
    assert_eq!(
        admit(&p, &h, &flat(1_000, 100)),
        Ok(()),
        "warm leases are not capped"
    );
    h.begin_round(5);
    assert_eq!(admit(&p, &h, &cold), Ok(()), "the next round starts again");

    let mut full = HostState::new(Pool::full().cap);
    full.begin_round(0);
    for i in 0..3 {
        assert_eq!(admit(&p, &full, &cold), Ok(()));
        full.place(i, 0, &cold);
    }
    assert_eq!(admit(&p, &full, &cold), Err(Refusal::ColdCap));

    // Out of CPU as well: the refusal is CPU, the cold cap only when it stands alone.
    let mut busy = HostState::new(Pool::ci().cap);
    busy.begin_round(0);
    let whole = flat(16_000, 100);
    busy.place(0, 0, &whole);
    assert_eq!(admit(&p, &busy, &cold), Err(Refusal::Cpu { slot: 0 }));
}

/// Catches: a lease cap off by one, not per thread (64 at 16 threads, 352 at 88), or
/// checked after the resources.
#[test]
fn the_lease_cap_is_four_per_thread() {
    let p = Params::default();
    for (pool, cap) in [(Pool::ci(), 64usize), (Pool::full(), 352)] {
        let tiny = flat(1, 1);
        let mut h = HostState::new(pool.cap);
        h.begin_round(0);
        for i in 0..cap {
            assert_eq!(admit(&p, &h, &tiny), Ok(()), "lease {i}");
            h.place(i, 0, &tiny);
        }
        assert_eq!(admit(&p, &h, &tiny), Err(Refusal::LeaseCap));
        // Over CPU too: the lease cap is what is reported.
        assert_eq!(
            admit(&p, &h, &flat(pool.cap.cpu_millis, 1)),
            Err(Refusal::LeaseCap)
        );
    }
}

/// Catches: a charge larger than the host charged as it is (the shipped predicate clamps,
/// so a key that wants more than a host runs on an otherwise idle one): admitted alone,
/// refused beside anything.
#[test]
fn a_charge_over_the_host_is_clamped_to_it() {
    let p = Params::default();
    let big = flat(40_000, 200_000);
    let cap = Pool::ci().cap;
    let h = HostState::new(cap);
    assert_eq!(admit(&p, &h, &big), Ok(()));
    let mut h = HostState::new(cap);
    let small = flat(2_000, 100);
    h.place(0, 0, &small);
    assert_eq!(admit(&p, &h, &big), Err(Refusal::Cpu { slot: 0 }));
    let mut h = HostState::new(cap);
    h.place(0, 0, &big);
    assert_eq!(
        h.sums().slots[0],
        Charge::new(16_000, 46_694),
        "charged the host's size"
    );
    h.leave(0);
    assert_eq!(*h.sums(), HostState::new(cap).sums().clone());
}

/// Catches: sums that drift from the leases they were built from: after a random run of
/// placements, departures and the passing of time (so every resident is re-charged each
/// minute), the incremental sums equal the sums computed from scratch, and each
/// resident's age is its start's.
#[test]
fn incremental_sums_equal_the_rebuilt_ones() {
    let mut rng = SimRng::from_seed(7);
    let beliefs: Vec<Belief> = (0..12)
        .map(|i| {
            let n = rng.below(40) as usize;
            let curve: Vec<Charge> = (0..n)
                .map(|_| Charge::new(rng.below(30_000), rng.below(60_000)))
                .collect();
            Belief::new(
                i % 3 == 0,
                1 + rng.below(30) as u8,
                Charge::new(rng.below(9_000), rng.below(9_000)),
                curve,
            )
        })
        .collect();
    let mut host = HostState::new(Pool::ci().cap);
    let mut next = 0usize;
    let mut checked_old = 0;
    for now in 0..6_000u64 {
        host.tick(now);
        if now % 5 == 0 {
            host.begin_round(now);
            if rng.below(2) == 0 && host.residents().len() < 40 {
                let b = &beliefs[rng.below(beliefs.len() as u64) as usize];
                host.place(next, now, b);
                next += 1;
            }
        }
        if rng.below(150) == 0 {
            let leases: Vec<usize> = host.residents().keys().copied().collect();
            if !leases.is_empty() {
                host.leave(leases[rng.below(leases.len() as u64) as usize]);
            }
        }
        assert_eq!(*host.sums(), host.rebuild(now), "tick {now}");
        for r in host.residents().values() {
            assert_eq!(r.age, (now - r.start_tick) / 60);
            checked_old += u64::from(r.age >= 3);
        }
    }
    assert!(
        checked_old > 1_000,
        "residents grew old enough to be re-charged"
    );
}
