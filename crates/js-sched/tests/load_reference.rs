//! The load reference (`load_reference/mod.rs`) against the figures of
//! `docs/design/placement-by-load.md`, worked by hand.
//!
//! The fleet simulations hold the scheduler to the reference, so a reference that bends
//! the same way the scheduler does would pass them. Each test here fixes one rule of the
//! document on a small fleet whose answer is computed from the document's formulas, with
//! no call into `js-sched`. The cases are all flat, as the reference is: every request
//! asks for cores only (a core or more, so the unmeasured floor changes nothing unless a
//! test is about it), and no worker measures.
//!
//! The tie-breaks of the document, in order, and the test that fixes each:
//!
//! 1. fewer uncovered leases and 2. the larger worker: `ties_go_to_fewer_leases_then_the_larger_worker`;
//! 3. the larger headroom: `a_tie_goes_to_the_larger_headroom_before_the_hash`;
//! 4. the hash of lease and worker: `identical_workers_are_told_apart_by_the_hash_of_lease_and_worker`
//!    (and the encoding in `the_tie_break_hash_is_length_prefixed_fnv_1a`);
//! 5. the first by name, which the code takes when the hashes are equal. The reference only
//!    reaches it for a request that owns every candidate whole
//!    (`an_action_that_owns_a_worker_whole_is_not_scored`); two distinct workers never hash alike.

#[path = "load_reference/mod.rs"]
mod load_reference;

use std::collections::BTreeMap;

use js_types::sched::{OperationId, Resources, WorkKey, WorkerId};
use load_reference::{Ask, Host, Placer, fnv1a, tie_hash};

const GIB: u64 = 1 << 30;

fn key(n: u64) -> WorkKey {
    WorkKey::new("main", format!("k{n}"))
}

/// A worker of `cores` cores and `gib` GiB holding a lease of each of `charged` millicores,
/// every lease of its own key.
fn host(name: &str, cores: u64, gib: u64, charged: &[u64]) -> Host {
    let capacity = Resources::new(cores * 1_000, gib * GIB);
    let leases = charged
        .iter()
        .enumerate()
        .map(|(i, cpu)| (key(1_000 + i as u64), *cpu))
        .collect();
    Host::new(WorkerId::new(name), capacity, 0, 0, false, leases)
}

/// A worker whose leases are all of `shared`.
fn host_of_key(name: &str, cores: u64, shared: &WorkKey, charged: &[u64]) -> Host {
    let capacity = Resources::new(cores * 1_000, 32 * GIB);
    let leases = charged.iter().map(|cpu| (shared.clone(), *cpu)).collect();
    Host::new(WorkerId::new(name), capacity, 0, 0, false, leases)
}

fn cores(millis: u64) -> Resources {
    Resources::new(millis, GIB)
}

fn placer(hosts: Vec<Host>, next_seq: u64) -> Placer {
    Placer::new(hosts, 1, next_seq, BTreeMap::new())
}

/// Places the request `operation` of `key` for `resources` among `candidates` of `pool`.
fn place(
    p: &mut Placer,
    operation: u64,
    key: &WorkKey,
    resources: Resources,
    pool: usize,
    candidates: &[usize],
) -> Option<usize> {
    p.place(&Ask {
        operation: OperationId(operation),
        key,
        resources,
        pool,
        candidates,
    })
}

/// Catches: FNV-1a with the wrong basis or prime, a length prefix of another width or byte
/// order, the lease id in the `1.2` display form, or the worker hashed before the lease.
#[test]
fn the_tie_break_hash_is_length_prefixed_fnv_1a() {
    // The standard FNV-1a vectors.
    assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    // The lease `1-2` and the worker `w1`, each preceded by its length as a `u32` in
    // little-endian bytes (computed here from the document's encoding).
    assert_eq!(tie_hash(1, 2, &WorkerId::new("w1")), 0x922e_fe9b_031d_ce50);
}

/// Catches: the limit taken as the capacity (no slack), the slack applied twice, a boundary
/// that refuses what exactly fills it, and the floor of a core missing: a worker of 10
/// cores is planned to 11, a lease of 100 millicores is charged a core, and a worker of
/// half a core charges at most what it has.
#[test]
fn a_worker_is_planned_to_a_tenth_over_its_cores() {
    let full = host("a", 10, 32, &[1_000; 10]);
    assert_eq!(full.cpu_limit(), 11_000);
    assert_eq!(full.left(), 10_000);
    assert!(full.admits(&cores(1_000)), "10 + 1 cores fills it exactly");
    assert!(!full.admits(&cores(1_001)), "a millicore more is refused");
    // Ten leases of 100 millicores are charged a core each.
    let light = host("a", 10, 32, &[100; 10]);
    assert_eq!(light.left(), 10_000);
    assert!(!light.admits(&cores(2_000)));
    assert!(light.admits(&cores(100)), "which is a core: 11 of 11");
    // A worker of half a core: the floor is cut to its cores.
    let tiny = Host::new(
        WorkerId::new("t"),
        Resources::new(500, GIB),
        0,
        0,
        false,
        vec![(key(1), 500)],
    );
    assert_eq!(tiny.left(), 500);
    assert_eq!(tiny.cpu_limit(), 550);
    assert!(!tiny.admits(&Resources::new(500, 0)), "1000 > 550");
}

/// Catches: room that ignores a worker held whole, GPUs or VM slots not booked, a request
/// for more memory or cores than the worker has, and an action at the whole host's
/// memory placed beside another lease.
#[test]
fn room_is_what_the_load_does_not_decide() {
    let capacity = Resources::new(8_000, 16 * GIB).with_gpus(2).with_vms(1);
    let mut h = Host::new(WorkerId::new("a"), capacity, 1, 1, false, Vec::new());
    assert!(h.has_room(&cores(1_000).with_gpus(1)));
    assert!(!h.has_room(&cores(1_000).with_gpus(2)), "one GPU is booked");
    assert!(
        !h.has_room(&cores(1_000).with_vms(1)),
        "the VM slot is booked"
    );
    assert!(
        !h.has_room(&Resources::new(9_000, GIB)),
        "more cores than it has"
    );
    assert!(!h.has_room(&Resources::new(1_000, 17 * GIB)), "more memory");
    // The whole host's memory runs alone.
    let whole = Resources::new(1_000, 16 * GIB);
    assert!(h.has_room(&whole));
    h.book(&key(1), &cores(1_000));
    assert!(!h.has_room(&whole), "a lease is already there");
    let mut alone = Host::new(WorkerId::new("b"), capacity, 0, 0, false, Vec::new());
    alone.book(&key(2), &whole);
    assert!(!alone.has_room(&cores(1_000)), "nothing beside it");
}

/// Catches: key 1 missing or after the fit: a worker predicted past 100% of its cores (by
/// more than the tolerance, 1% of its capacity) must lose to one that fits inside, though
/// the fit prefers the fuller worker; and an overshoot inside the tolerance must not count.
#[test]
fn the_slack_zone_is_used_only_when_no_worker_fits_inside_a_hundred_percent() {
    let k = key(1);
    // 10-core workers. `a` is at 9 cores: a lease of 2 takes it to 11 (110%), which the
    // predicate admits and the score avoids while `b` (empty) takes it inside 100%.
    let mut p = placer(vec![host("a", 10, 32, &[9_000]), host("b", 10, 32, &[])], 0);
    assert_eq!(place(&mut p, 1, &k, cores(2_000), 0, &[0, 1]), Some(1));
    // With `b` at 9 cores too, both would be at 110%: the slack is used, nothing tells them
    // apart but the hash, and `a` wins it for lease 1-0.
    let mut p = placer(
        vec![host("a", 10, 32, &[9_000]), host("b", 10, 32, &[9_000])],
        0,
    );
    assert_eq!(place(&mut p, 1, &k, cores(2_000), 0, &[0, 1]), Some(0));
    // An overshoot of 50 millicores, half the tolerance: `a` at 9.05 cores plus a core is
    // 10.05. Key 1 is 0 for both, and the fit prefers the fuller `a`.
    let mut p = placer(vec![host("a", 10, 32, &[9_050]), host("b", 10, 32, &[])], 0);
    assert_eq!(place(&mut p, 1, &k, cores(1_000), 0, &[0, 1]), Some(0));
    // An overshoot of 150: past the tolerance, so `b` wins on key 1.
    let mut p = placer(vec![host("a", 10, 32, &[9_150]), host("b", 10, 32, &[])], 0);
    assert_eq!(place(&mut p, 1, &k, cores(1_000), 0, &[0, 1]), Some(1));
}

/// Catches: the fit not squaring (a linear fit prefers the emptier worker), the stranded
/// memory term dropped, and the binding resource taken as the larger leftover: with no
/// memory charged the memory leftover is whole, so the CPU leftover binds, and
/// `2 x^2 + (1 - x)^2` (x the leftover share of cores) is least at a third.
#[test]
fn the_fit_prefers_the_worker_that_leaves_about_a_third_of_its_cores() {
    // 10 cores each, a lease of 1 core: after it the leftovers are 70%, 40% and 20%, with
    // scores 2(.49) + .09 = 1.07, 2(.16) + .36 = .68 and 2(.04) + .64 = .72.
    let hosts = vec![
        host("a", 10, 32, &[2_000]),
        host("b", 10, 32, &[5_000]),
        host("c", 10, 32, &[7_000]),
    ];
    let mut p = placer(hosts, 0);
    assert_eq!(
        place(&mut p, 1, &key(1), cores(1_000), 0, &[0, 1, 2]),
        Some(1)
    );
}

/// Catches: the key share ignored, taken as a refusal, measured against the wrong capacity,
/// strict where the document says "exceed", or a share other than a quarter: a worker that
/// already holds enough of the lease's key to pass 25% of its cores loses to one that does
/// not, though it is the fuller and the fit prefers it; exactly 25% is not passing it, and
/// 26% is.
#[test]
fn a_worker_concentrated_on_the_key_is_ranked_below_one_that_is_not() {
    let shared = key(7);
    // 10-core workers. `a` holds three leases of the key (3 cores); another core makes 4,
    // 40% of 10 cores, past 25%. `b` is emptier and holds none of it. The fit prefers `a`
    // (leftover 60%: .88) to `b` (80%: 1.32); the share puts `b` first.
    let a = host_of_key("a", 10, &shared, &[1_000, 1_000, 1_000]);
    let b = host("b", 10, 32, &[1_000]);
    let mut p = placer(vec![a, b], 0);
    assert_eq!(place(&mut p, 1, &shared, cores(1_000), 0, &[0, 1]), Some(1));
    // 10-core workers. `a` holds 1.5 cores of the key, so 2.5 cores with the new one are
    // exactly 25% and not concentrated; `b` holds 1.6, so 2.6 cores (26%) are. `b` is the
    // slightly fuller and the fit prefers it, but only `b` is concentrated: `a` wins.
    let a = host_of_key("a", 10, &shared, &[1_500]);
    let b = host_of_key("b", 10, &shared, &[1_600]);
    let mut p = placer(vec![a, b], 0);
    assert_eq!(place(&mut p, 1, &shared, cores(1_000), 0, &[0, 1]), Some(0));
}

/// Catches: the first tie-break (fewer leases) missing or after the second, and the
/// second (the larger worker) missing: the workers are built so that keys 1 to 5 tie.
#[test]
fn ties_go_to_fewer_leases_then_the_larger_worker() {
    // `a`: 10 cores with one lease of 1 core, `b`: 20 cores with one of 3: a lease of a
    // core leaves each at 80%. Keys 1 to 5 tie and the leases (1 each); the larger, `b`.
    let mut p = placer(
        vec![host("a", 10, 32, &[1_000]), host("b", 20, 32, &[3_000])],
        0,
    );
    assert_eq!(place(&mut p, 1, &key(1), cores(1_000), 0, &[0, 1]), Some(1));
    // `a`: 10 cores with one lease of 2 cores, `b`: 20 cores with three leases of 1, 1 and
    // 3 cores: each at 70% after a core, but `a` holds fewer leases and wins though `b` is
    // the larger.
    let a = host("a", 10, 32, &[2_000]);
    let b = host("b", 20, 32, &[1_000, 1_000, 3_000]);
    let mut p = placer(vec![a, b], 0);
    assert_eq!(place(&mut p, 1, &key(1), cores(1_000), 0, &[0, 1]), Some(0));
}

/// Catches: the last tie-break missing (the first by name always wins), the hash taken
/// over the wrong lease number (the round's next number, and one more for each lease
/// placed before it), or compared the wrong way. For identical empty workers `a` and `b`
/// the hash of lease `1-n` and the worker names gives `a`, `b`, `b`, `a` for n = 0 to 3;
/// for `c` and `d`, `c`, `c`, `d`, `c`.
#[test]
fn identical_workers_are_told_apart_by_the_hash_of_lease_and_worker() {
    let fresh = || {
        vec![
            host("a", 64, 64, &[]),
            host("b", 64, 64, &[]),
            host("c", 64, 64, &[]),
            host("d", 64, 64, &[]),
        ]
    };
    for (n, want) in [0, 1, 1, 0].into_iter().enumerate() {
        let mut p = placer(fresh(), n as u64);
        assert_eq!(
            place(&mut p, 1, &key(1), cores(1_000), 0, &[0, 1]),
            Some(want)
        );
    }
    for (n, want) in [2, 2, 3, 2].into_iter().enumerate() {
        let mut p = placer(fresh(), n as u64);
        assert_eq!(
            place(&mut p, 1, &key(1), cores(1_000), 0, &[2, 3]),
            Some(want)
        );
    }
    // Within one round the second grant is numbered one after the first: lease 1-0 goes
    // among `c` and `d` to `c`, then lease 1-1 among `a` and `b` to `b` (it would be `a`
    // if the number did not advance).
    let mut p = placer(fresh(), 0);
    assert_eq!(place(&mut p, 1, &key(1), cores(1_000), 0, &[2, 3]), Some(2));
    assert_eq!(place(&mut p, 2, &key(2), cores(1_000), 0, &[0, 1]), Some(1));
}

/// Catches: a blocked head that holds nothing, one that holds the worker with the most
/// load, a held worker that still takes later work, and a second head of the same pool
/// holding another worker.
#[test]
fn a_blocked_head_holds_the_worker_it_would_leave_least_full() {
    // 10-core workers (limit 11 cores): `a` at 5, `b` at 6, `c` at 5.5. A request for 6.5
    // cores is refused by all: 11.5, 12.5 and 12 cores.
    let hosts = vec![
        host("a", 10, 32, &[5_000]),
        host("b", 10, 32, &[6_000]),
        host("c", 10, 32, &[5_500]),
    ];
    let mut p = placer(hosts, 0);
    let big = cores(6_500);
    assert_eq!(place(&mut p, 1, &key(1), big, 0, &[0, 1, 2]), None);
    // The head holds `a` (ratio 1.15, the least of 1.15, 1.25 and 1.2).
    assert_eq!(p.holds().get(&OperationId(1)), Some(&WorkerId::new("a")));
    // A small request behind it does not go to `a`, though `a` has the most room: of `b`
    // and `c` it goes to `c` on the fit (leftovers 30% and 35%: .67 and .6675).
    assert_eq!(
        place(&mut p, 2, &key(2), cores(1_000), 0, &[0, 1, 2]),
        Some(2)
    );
    // A second blocked request of the pool holds nothing more.
    assert_eq!(place(&mut p, 3, &key(3), big, 0, &[0, 1, 2]), None);
    assert_eq!(p.holds().len(), 1);
    // A blocked request of another pool holds its own worker, never `a`: of `b` (1.25)
    // and `c` (now at 6.5 cores: 1.3) it is `b`.
    assert_eq!(place(&mut p, 4, &key(4), big, 1, &[0, 1, 2]), None);
    assert_eq!(p.holds().get(&OperationId(4)), Some(&WorkerId::new("b")));
}

/// Catches: a hold that moves for any wobble (the kept worker is dropped as soon as another
/// is better), one that never moves, and a band that is not exactly a tenth: the kept
/// worker stays while its binding ratio is at most the best's plus 0.1.
#[test]
fn the_held_worker_is_kept_while_within_a_tenth_of_the_best() {
    // 10-core workers; a request for 6.5 cores. `b` is the best, at 11.2 / 10 = 1.12
    // (refused: above 1.1). The kept `a` at 5.7 cores is 12.2 / 10 = 1.22, exactly 0.1 over.
    let kept = BTreeMap::from([(OperationId(1), WorkerId::new("a"))]);
    let held = |a_charged: u64| {
        let hosts = vec![host("a", 10, 32, &[a_charged]), host("b", 10, 32, &[4_700])];
        let mut p = Placer::new(hosts, 1, 0, kept.clone());
        assert_eq!(place(&mut p, 1, &key(1), cores(6_500), 0, &[0, 1]), None);
        p.holds().get(&OperationId(1)).cloned().unwrap()
    };
    assert_eq!(held(5_700), WorkerId::new("a"), "exactly a tenth: kept");
    assert_eq!(held(5_701), WorkerId::new("b"), "past a tenth: it moves");
    // A kept worker that is not a candidate is not held.
    let hosts = vec![host("a", 10, 32, &[5_700]), host("b", 10, 32, &[4_700])];
    let mut p = Placer::new(hosts, 1, 0, kept);
    assert_eq!(place(&mut p, 1, &key(1), cores(6_500), 0, &[1]), None);
    assert_eq!(p.holds().get(&OperationId(1)), Some(&WorkerId::new("b")));
}

/// Catches: an action at the whole host's memory scored against workers that stay shared
/// (it goes to a shared one while one admits it, and to the first by name when every
/// worker would be owned whole), or placed beside another lease.
#[test]
fn an_action_that_owns_a_worker_whole_is_not_scored() {
    // `a` has 8 GiB, `b` and `c` have 32, all empty and alike but in memory. A request for
    // 8 GiB owns `a` whole and shares `b` and `c`: only those are scored, and they tie to
    // the last tie-break, the hash of lease 1-0, which `c` wins over `b`. (Scored with `a`,
    // `a` would win it.)
    let req = Resources::new(1_000, 8 * GIB);
    let hosts = vec![
        host("a", 10, 8, &[]),
        host("b", 10, 32, &[]),
        host("c", 10, 32, &[]),
    ];
    let mut p = placer(hosts, 0);
    assert_eq!(place(&mut p, 1, &key(1), req, 0, &[0, 1, 2]), Some(2));
    // A single worker that stays shared is taken as it is, though others would be owned.
    let hosts = vec![host("a", 10, 8, &[]), host("b", 10, 32, &[3_000])];
    let mut p = placer(hosts, 0);
    assert_eq!(place(&mut p, 1, &key(1), req, 0, &[0, 1]), Some(1));
    // With `a` and `d` (both 8 GiB) it owns either: the first by name.
    let hosts = vec![host("a", 10, 8, &[]), host("d", 10, 8, &[])];
    let mut p = placer(hosts, 0);
    assert_eq!(place(&mut p, 1, &key(1), req, 0, &[0, 1]), Some(0));
    // And nothing is placed beside it.
    assert_eq!(place(&mut p, 2, &key(2), cores(1_000), 0, &[0]), None);
}

/// Catches: the limit rounded up (or to the nearest) instead of down: `C (1 + s_cpu)` of
/// 505 millicores is 555.5, planned as 555, and of 10,005 is 11,005.5, planned as 11,005.
/// On the second the leases are charged 10,005 millicores in all, so a request of 1,000
/// reaches the limit exactly and is admitted while one millicore more is refused (a limit
/// of 11,006 would admit it).
#[test]
fn the_limit_is_rounded_down_to_a_millicore() {
    let half = Host::new(
        WorkerId::new("h"),
        Resources::new(505, GIB),
        0,
        0,
        false,
        Vec::new(),
    );
    assert_eq!(half.cpu_limit(), 555);
    // An empty worker of 505 millicores charges a lease its 505 (the floor is cut to its
    // cores): 505 <= 555.
    assert!(half.admits(&Resources::new(505, 0)));
    let odd = Host::new(
        WorkerId::new("o"),
        Resources::new(10_005, 32 * GIB),
        0,
        0,
        false,
        vec![(key(1), 5_000), (key(2), 5_005)],
    );
    assert_eq!(odd.cpu_limit(), 11_005);
    assert_eq!(odd.left(), 10_005);
    assert!(odd.admits(&Resources::new(1_000, GIB)), "11,005 fills it");
    assert!(
        !odd.admits(&Resources::new(1_001, GIB)),
        "11,006 is past it"
    );
}

/// Catches: the third tie-break (the larger headroom, the cores the worker has over those
/// charged) missing or taken the wrong way. Two workers of 10 cores each hold one lease
/// and take the request to 10,050 and 10,000 millicores (`a`, charged 9,050, ends 50 over;
/// `b`, charged 9,000, ends exactly full): both have a leftover of 0 and an overshoot inside the
/// tolerance, so keys 1 to 5 and the first two tie-breaks are equal, and `b` has the
/// larger headroom (1,000 against 950). The hash prefers `a` for the leases 1-0 and 1-3
/// (see `identical_workers_are_told_apart_by_the_hash_of_lease_and_worker`), so `b` wins
/// for them only through the headroom.
#[test]
fn a_tie_goes_to_the_larger_headroom_before_the_hash() {
    for n in 0..4 {
        let mut p = placer(
            vec![host("a", 10, 32, &[9_050]), host("b", 10, 32, &[9_000])],
            n,
        );
        assert_eq!(
            place(&mut p, 1, &key(1), cores(1_000), 0, &[0, 1]),
            Some(1),
            "lease 1-{n}"
        );
    }
}
