//! A worker whose fresh reading carries the whole host's memory in use is charged that
//! figure for the leases the reading covers, and the bookings of the leases it does not
//! cover yet, by the predicate and by the flat-memory guard alike; a refusal on memory
//! carries the figure it used. The worker is of 100 cores and 32 GiB.

#[path = "profile_rig/mod.rs"]
mod profile_rig;

use js_sched::{H_PLUS, LoadConfig, Refusal, RefusalKind};
use js_types::sched::{LeaseId, LeaseKind, Resources};

use profile_rig::load_rig::{self, GIB, Rig, W_MS, w, warm};
use profile_rig::{cluster_on, flat_ws, place};

/// The worker of these tests: 100 cores, 32 GiB, `vms` VM slots.
fn worker(vms: u64) -> Resources {
    Resources::new(100_000, 32 * GIB).with_vms(vms)
}

/// One window passes on `name`: a reading of 1 core busy, nothing in the actions cgroup's
/// working set, `covered` leases, and the host's memory in use `host_used`.
fn window(r: &mut Rig, name: &str, host_used: Option<u64>, covered: &[LeaseId]) {
    r.ms += W_MS;
    let mut load = r.reading(name, 1_000, 0, covered.to_vec());
    load.host_memory_used_bytes = host_used;
    r.beat(name, Some(load));
}

/// A rig under `config` whose worker `a` (with `vms` VM slots and the VM driver) holds three
/// leases charged 10 GiB each (30 of its 32 GiB; each runs under a limit of 1 GiB, so a VM's
/// booked-capacity rule has room beside them), all covered by a fresh reading that measured
/// `host_used` in use on the host.
fn thirty_booked(config: LoadConfig, vms: u64, host_used: Option<u64>) -> Rig {
    let mut r = Rig::new(config);
    let caps = load_rig::caps("container").with_drivers(["container", "vm"]);
    r.up_with("a", worker(vms), caps);
    r.baseline("a");
    let held: Vec<LeaseId> = (0..3)
        .map(|_| {
            r.action(1_000, GIB, Some(warm(1_000, 10 * GIB)));
            r.tick()[0].lease
        })
        .collect();
    window(&mut r, "a", host_used, &held);
    window(&mut r, "a", host_used, &held);
    r
}

/// The memory refusal of the latest round, if any.
fn memory_refusal(r: &mut Rig) -> Option<Refusal> {
    let refusals = r.s.take_refusals();
    refusals
        .kept
        .into_iter()
        .map(|kept| kept.refusal)
        .find(|refusal| refusal.kind != RefusalKind::Cpu)
}

/// Behaviour: with the predicate's memory slack at 50% (so the flat-memory guard, at the
/// host's 32 GiB, is the bound that holds), a worker whose leases are charged 30 GiB but whose
/// host measured 8 GiB in use takes a lease of 24 GiB (8 + 24 = 32, the exact limit), and one
/// of 24 GiB and a byte more is refused by the guard with the measured figure: counted 8 GiB,
/// measured 8 GiB. The same worker with no host memory in its reading is held to the 30 GiB of
/// bookings, and a lease of 3 GiB is refused by the guard with no measurement. Catches the
/// guard summing every lease's flat cost on a host whose reading measured its memory, which
/// refuses hosts booked full while half idle.
#[test]
fn the_flat_memory_guard_counts_the_measured_memory_and_carries_it() {
    let config = LoadConfig {
        s_mem_permille: 500,
        ..LoadConfig::default()
    };
    let mut r = thirty_booked(config, 0, Some(8 * GIB));
    r.action(1_000, 24 * GIB + 1, Some(warm(1_000, 24 * GIB + 1)));
    assert!(r.tick().is_empty());
    let refusal = memory_refusal(&mut r).expect("refused for memory");
    assert_eq!(
        refusal,
        Refusal {
            kind: RefusalKind::BookedMemory,
            counted: 8 * GIB,
            new: 24 * GIB + 1,
            limit: 32 * GIB,
            measured_cpu_millis: None,
            measured_memory_bytes: Some(8 * GIB),
        }
    );

    let mut r = thirty_booked(config, 0, Some(8 * GIB));
    r.action(1_000, 24 * GIB, Some(warm(1_000, 24 * GIB)));
    assert_eq!(r.tick().len(), 1, "8 + 24 GiB is the limit exactly");

    let mut r = thirty_booked(config, 0, None);
    r.action(1_000, 3 * GIB, Some(warm(1_000, 3 * GIB)));
    assert!(r.tick().is_empty());
    let refusal = memory_refusal(&mut r).expect("refused for memory");
    assert_eq!(
        (refusal.kind, refusal.counted, refusal.measured_memory_bytes),
        (RefusalKind::BookedMemory, 30 * GIB, None)
    );
}

/// Behaviour: under the default configuration a worker whose leases are charged 30 GiB and
/// whose host measured 8 GiB in use is refused a lease of 25 GiB by the predicate, counted
/// 8 GiB with the measurement carried, and a lease placed after that reading is charged its
/// booking beside the measurement (8 + 20 GiB counted for the next one). Catches a refusal
/// that prints no measurement for a host whose memory was measured, and a lease placed since
/// the reading counted as if the reading saw it.
#[test]
fn the_predicate_counts_the_measurement_and_the_leases_placed_since() {
    let mut r = thirty_booked(LoadConfig::default(), 0, Some(8 * GIB));
    r.action(1_000, 25 * GIB, Some(warm(1_000, 25 * GIB)));
    assert!(r.tick().is_empty());
    let refusal = memory_refusal(&mut r).expect("refused for memory");
    assert_eq!(
        (
            refusal.kind,
            refusal.counted,
            refusal.limit,
            refusal.measured_memory_bytes
        ),
        (
            RefusalKind::PredictedMemory,
            8 * GIB,
            32 * GIB,
            Some(8 * GIB)
        )
    );

    let mut r = thirty_booked(LoadConfig::default(), 0, Some(8 * GIB));
    r.action(1_000, 20 * GIB, Some(warm(1_000, 20 * GIB)));
    assert_eq!(r.tick().len(), 1);
    r.action(1_000, 5 * GIB, Some(warm(1_000, 5 * GIB)));
    assert!(r.tick().is_empty());
    let refusal = memory_refusal(&mut r).expect("refused for memory");
    assert_eq!(
        (refusal.counted, refusal.measured_memory_bytes),
        (28 * GIB, Some(8 * GIB))
    );
}

/// Behaviour: a VM, which has no predicate and is held to the flat-memory guard, is judged by
/// the measured memory too: beside leases charged 30 GiB on a host that measured 4 GiB in use,
/// a VM booking 16 GiB is placed; with no host memory in the reading the same VM waits, since
/// 30 + 16 GiB of bookings is over 32. Catches a VM judged by the bookings on a host whose
/// reading measured its memory.
#[test]
fn a_vm_is_held_to_the_measured_memory_too() {
    for (host_used, placed) in [(Some(4 * GIB), 1), (None, 0)] {
        let mut r = thirty_booked(LoadConfig::default(), 1, host_used);
        let mut vm = r.request(LeaseKind::Vm, 4_000, 16 * GIB);
        vm.resources = vm.resources.with_vms(1);
        r.submit(vm);
        assert_eq!(r.tick().len(), placed, "host used {host_used:?}");
    }
}

/// Behaviour: leases charged by their memory curves are held to the measured memory as flat
/// ones are. A worker holds three leases whose curves ask 10 GiB each (30 of its 32 GiB), all
/// covered by a reading that measured 8 GiB in use on the host: it is charged 8 GiB at every
/// minute of the horizon and beyond it, a lease whose curve asks 24 GiB is placed (8 + 24 is
/// the limit), and one whose curve asks 25 GiB is refused over the horizon with the
/// measurement carried. Catches the horizon's memory check summing the curves of covered
/// leases on a host whose reading measured its memory, and a measurement that enters at the
/// first minute only.
#[test]
fn leases_charged_by_curve_are_held_to_the_measured_memory() {
    let start = |big: u64| {
        let (mut r, curves) = cluster_on(LoadConfig::default(), worker(0));
        curves.set("big", &flat_ws(&[1_000], big), 5);
        let held: Vec<LeaseId> = ["h0", "h1", "h2"]
            .into_iter()
            .map(|key| {
                curves.set(key, &flat_ws(&[1_000], 10 * GIB), 5);
                place(&mut r, key, warm(1_000, 10 * GIB)).lease
            })
            .collect();
        window(&mut r, "a", Some(8 * GIB), &held);
        window(&mut r, "a", Some(8 * GIB), &held);
        r
    };
    let r = start(24 * GIB);
    for slot in [0, 15, H_PLUS] {
        let (_, mem) = r.s.load_at(&w("a"), slot).expect("registered");
        assert_eq!(mem, 8 * GIB, "slot {slot}");
    }
    let mut r = start(24 * GIB);
    r.keyed("big", 1_000, 8 * GIB, warm(1_000, 24 * GIB));
    assert_eq!(r.tick().len(), 1, "8 + 24 GiB is the limit exactly");

    let mut r = start(25 * GIB);
    r.keyed("big", 1_000, 8 * GIB, warm(1_000, 25 * GIB));
    assert!(r.tick().is_empty());
    let refusal = memory_refusal(&mut r).expect("refused for memory");
    assert_eq!(
        (
            refusal.kind,
            refusal.counted,
            refusal.new,
            refusal.measured_memory_bytes
        ),
        (
            RefusalKind::PredictedMemory,
            8 * GIB,
            25 * GIB,
            Some(8 * GIB)
        )
    );
}
