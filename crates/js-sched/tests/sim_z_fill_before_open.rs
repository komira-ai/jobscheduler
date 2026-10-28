//! Sim z: fill before open. Two 88-thread hosts; the score takes a lease to the host it
//! fits most tightly (key 5), keeps off a host's 10% slack while another host takes the
//! lease inside 100% (key 1, above key 5), and keeps off a host with no room for the
//! memory miss while another has it (key 2, an ordering that never refuses).
//!
//! Logical time only; no assertion reads a clock.

#[path = "load_rig/mod.rs"]
mod load_rig;
#[path = "support_score_z_ad/mod.rs"]
mod support_score_z_ad;

use js_sched::{Event, Prediction};

use load_rig::{GIB, Rig, cold, w, warm};
use support_score_z_ad::{hold, host, on, place};

/// A flat 4-core lease with no memory of note.
fn four_cores() -> Prediction {
    warm(4_000, 0)
}

/// `A` holds 40 cores flat (one lease), `B` is empty; hosts of 88 threads.
fn rig_with_forty_on_a() -> Rig {
    let mut r = Rig::on();
    r.up("A", host(352));
    r.up("B", host(352));
    hold(&mut r, "A", &["B"], 1, warm(40_000, 0));
    r
}

/// Key 5 and key 1: the first 12 four-core leases go to `A` (88 cores: best fit toward
/// 100%), the 13th goes to `B` and not into `A`'s 10% slack (92 cores, 4.5% above `tol`),
/// and the leases after it stay on `B`, which still has room inside 100%.
#[test]
fn twelve_leases_fill_a_to_100_percent_and_the_13th_opens_b() {
    let mut r = rig_with_forty_on_a();
    for n in 1..=12 {
        assert_eq!(place(&mut r, four_cores()), "A", "lease {n} fills A");
    }
    assert_eq!(r.s.load_view(&w("A")).unwrap().left_cpu_millis, 88_000);
    assert_eq!(r.s.load_view(&w("B")).unwrap().left_cpu_millis, 0);
    for n in 13..=16 {
        assert_eq!(place(&mut r, four_cores()), "B", "lease {n} opens B");
    }
    assert_eq!(r.s.load_view(&w("A")).unwrap().left_cpu_millis, 88_000);
    assert_eq!(r.s.load_view(&w("B")).unwrap().left_cpu_millis, 16_000);
    assert_eq!(
        r.s.load_counters().chose_slack,
        0,
        "no lease was placed in a slack zone"
    );
}

/// Key 1 is what keeps the 13th out of `A`'s slack: the predicate admits it there. With
/// `B` cordoned for the round, `A` is the only host left and takes the 13th at 92 cores.
#[test]
fn the_predicate_admits_the_13th_lease_into_as_slack() {
    let mut r = rig_with_forty_on_a();
    hold(&mut r, "A", &["B"], 12, four_cores());
    assert_eq!(r.s.load_view(&w("A")).unwrap().left_cpu_millis, 88_000);
    r.feed(Event::Cordon { worker: w("B") });
    assert_eq!(place(&mut r, four_cores()), "A");
    assert_eq!(r.s.load_view(&w("A")).unwrap().left_cpu_millis, 92_000);
}

/// Hosts of 100 GiB with `A` holding `a_gib` and `B` holding `b_gib` of predicted
/// memory, one 76-core lease each, so that CPU (80 cores with the new lease) leaves
/// about as much unused as memory does and the fit is decided by the memory leftover.
fn memory_pair(a_gib: u64, b_gib: u64) -> Rig {
    let mut r = Rig::on();
    r.up("A", host(100));
    r.up("B", host(100));
    hold(&mut r, "A", &["B"], 1, warm(76_000, a_gib * GIB));
    hold(&mut r, "B", &["A"], 1, warm(76_000, b_gib * GIB));
    r
}

/// Key 2 above key 5. `A` holds 65 GiB and fits a 30 GiB lease better (5 GiB left
/// against 50), but `65 + 1.3 * 30 = 104` is over 100 GiB. `B` holds 20 GiB and has room
/// for the miss (`20 + 39 = 59`). The memory-heavy lease goes to `B`.
#[test]
fn a_memory_heavy_lease_goes_to_the_host_with_room_for_the_miss() {
    let mut r = memory_pair(65, 20);
    assert_eq!(place(&mut r, warm(4_000, 30 * GIB)), "B");
}

/// Key 2 never refuses. `A` holds 65 GiB and `B` 62 GiB: neither has room for the 39 GiB
/// miss of a 30 GiB lease (`104` and `101` against 100), both pass the predicate. The
/// lease is placed, on the host it fits better (`A`, 5 GiB left against 8).
#[test]
fn with_no_room_for_the_miss_anywhere_the_lease_goes_to_the_better_fit() {
    let mut r = memory_pair(65, 62);
    assert_eq!(place(&mut r, warm(4_000, 30 * GIB)), "A");
}

/// Cold mass event on a memory-bound pool: three hosts of 20 GiB, `A` holding 16 GiB,
/// `B` 8 GiB, `C` nothing. Cold 4 GiB leases (`1.63 * 4 = 6.52` GiB of miss) go first to
/// the hosts that have that room: `B` while `8 + 6.52` and `12 + 6.52` fit, then `C`,
/// never `A` (16 GiB held, which fits a 4 GiB lease exactly and so fits best).
#[test]
fn cold_leases_spread_over_the_hosts_with_room_before_any_packs() {
    let mut r = Rig::on();
    for name in ["A", "B", "C"] {
        r.up(name, host(20));
    }
    hold(&mut r, "A", &["B", "C"], 4, warm(1_000, 4 * GIB));
    hold(&mut r, "B", &["A", "C"], 2, warm(1_000, 4 * GIB));
    let mut starts = Vec::new();
    for _ in 0..5 {
        r.action(1_000, 4 * GIB, Some(cold(1_000, 4 * GIB)));
        starts.extend(r.tick());
    }
    assert_eq!(on(&starts), ["B", "B", "C", "C", "C"]);
}

/// The miss factor of a cold key is 1.63, of a warm one 1.3: with `A` at 16 GiB and
/// `B` at 14 GiB of 20, a 4 GiB lease has room on `B` for a 1.3 miss (`19.2`) and not for
/// a 1.63 miss (`20.52`). The warm lease goes to the fuller `B`; the cold one to the
/// empty `C`.
#[test]
fn a_cold_lease_is_held_to_the_larger_miss() {
    let go = |hint: Prediction| {
        let mut r = Rig::on();
        for name in ["A", "B", "C"] {
            r.up(name, host(20));
        }
        hold(&mut r, "A", &["B", "C"], 1, warm(1_000, 16 * GIB));
        hold(&mut r, "B", &["A", "C"], 1, warm(1_000, 14 * GIB));
        place(&mut r, hint)
    };
    assert_eq!(go(warm(1_000, 4 * GIB)), "B");
    assert_eq!(go(cold(1_000, 4 * GIB)), "C");
}
