//! A host whose CPUs are all busy and whose tasks queue for them: the measured term of the
//! CPU predicate is the cores demanded, busy plus waiting
//! (`NodeLoad::demand_millicores_since`), so it can read above the host's cores where busy
//! cores cannot.
//!
//! An 88-core host (96 800 m under the default CPU slack of 1.1) holds 25 leases predicted
//! at 2000 m (50 000 m), each covered by its readings. The rig feeds readings as the host's
//! heartbeats, a window being 5 s of farm time; the pressure counter is the `some` total of
//! the host's `/proc/pressure/cpu`, which counts from boot, so it starts above 0.

#[path = "load_rig/mod.rs"]
mod load_rig;

use js_sched::{HostRefusal, Refusal, RefusalKind};
use js_types::sched::{LeaseId, Resources};

use load_rig::{GIB, Rig, W_MS, leases, w, warm};

/// The host's pressure counter at the baseline, in microseconds: what it held at boot plus
/// the time some task waited since.
const SINCE_BOOT_USEC: u64 = 7_000_000_000;

/// An 88-core host with 256 GiB.
fn host() -> Resources {
    Resources::new(88_000, 256 * GIB)
}

/// One window on `a`: `busy` millicores busy over it, the pressure counter at `pressure`
/// microseconds, every lease of `covered` running the whole window.
fn window(r: &mut Rig, busy: u64, pressure: u64, covered: &[LeaseId]) {
    r.ms += W_MS;
    let mut load = r.reading("a", busy, 64 * GIB, covered.to_vec());
    load.cpu_pressure_some_total_usec = pressure;
    r.beat("a", Some(load));
}

/// The host `a` with its 25 leases placed and covered, a baseline, and one window of `busy`
/// millicores in which some task waited `waited` microseconds of the 5 s.
fn saturated(busy: u64, waited: u64) -> (Rig, Vec<LeaseId>) {
    let mut r = Rig::on();
    r.up("a", host());
    for _ in 0..25 {
        r.action(2_000, GIB, Some(warm(2_000, 0)));
    }
    let held = leases(&r.tick());
    assert_eq!(held.len(), 25);
    window(&mut r, 0, SINCE_BOOT_USEC, &held);
    window(&mut r, busy, SINCE_BOOT_USEC + waited, &held);
    (r, held)
}

/// Asks `a` for one more lease of 2000 m: whether it was placed, and the refusal it gave.
fn ask(r: &mut Rig) -> (bool, Option<Refusal>) {
    r.action(2_000, GIB, Some(warm(2_000, 0)));
    let placed = !r.tick().is_empty();
    let refused = r.s.take_refusals().kept.into_iter().next();
    (placed, refused.map(|HostRefusal { refusal, .. }| refusal))
}

/// Asserts that a host at 87 of 88 cores busy with a long run queue (some task waiting for a
/// CPU 950 thousandths of the window, as at a load of 5.5 to 6.2 per CPU) is full: it is
/// charged 87 000 m busy plus 82 650 m waiting, 169 650 m, and refuses a lease of 2000 m for
/// CPU with those figures, where a reading of busy cores alone (87 000 + 2000 under 96 800)
/// took it. Catches a measured term that is busy cores, which cannot exceed the host's cores.
#[test]
fn a_host_at_87_of_88_busy_with_a_long_run_queue_is_full() {
    let (mut r, _) = saturated(87_000, 4_750_000);
    let view = r.s.load_view(&w("a")).expect("registered");
    assert_eq!(
        (
            view.fallback,
            view.measured_cpu_millis,
            view.left_cpu_millis
        ),
        (false, Some(169_650), 169_650)
    );
    let refusal = Refusal {
        kind: RefusalKind::Cpu,
        counted: 169_650,
        new: 2_000,
        limit: 96_800,
        measured_cpu_millis: Some(169_650),
    };
    assert_eq!(ask(&mut r), (false, Some(refusal)));
    assert_eq!(r.s.load_counters().refused_cpu, 1);
}

/// Asserts that a host at 87 of 88 cores busy whose pressure counter did not move (nothing
/// waited, or an agent that sends no counter, which reads 0) is charged its busy cores alone
/// and takes the lease (89 000 m of 96 800): the waiting term adds only what waited. Catches
/// a host refused for being busy, and a pressure counter since boot read as one window's.
#[test]
fn a_busy_host_where_nothing_waited_is_charged_its_busy_cores() {
    let (mut r, _) = saturated(87_000, 0);
    assert_eq!(
        r.s.load_view(&w("a")).unwrap().measured_cpu_millis,
        Some(87_000)
    );
    assert_eq!(ask(&mut r), (true, None));

    let mut r = Rig::on();
    r.up("a", host());
    let held = leases(&r.tick());
    window(&mut r, 0, 0, &held);
    window(&mut r, 87_000, SINCE_BOOT_USEC, &held);
    assert_eq!(
        r.s.load_view(&w("a")).unwrap().measured_cpu_millis,
        Some(87_000)
    );
}

/// Asserts the boundary of the CPU limit with the waiting term: 80 000 m busy with some task
/// waiting 925 000 us of the 5 s is 94 800 m, and a lease of 2000 m lands exactly on 96 800 m
/// and is placed; 63 us more of waiting is 94 801 m, and it is refused. Catches a waiting term
/// rounded up, and the limit read as exclusive.
#[test]
fn a_lease_landing_exactly_on_the_limit_with_the_waiting_term_is_placed() {
    let (mut r, _) = saturated(80_000, 925_000);
    assert_eq!(
        r.s.load_view(&w("a")).unwrap().measured_cpu_millis,
        Some(94_800)
    );
    assert_eq!(ask(&mut r), (true, None));

    let (mut r, _) = saturated(80_000, 925_063);
    assert_eq!(
        r.s.load_view(&w("a")).unwrap().measured_cpu_millis,
        Some(94_801)
    );
    let (placed, refusal) = ask(&mut r);
    assert!(!placed);
    assert_eq!(refusal.map(|r| (r.counted, r.new)), Some((94_801, 2_000)));
}

/// Asserts that a full host takes work again once its queue has drained: after the window of
/// 169 650 m, the next window with nothing waiting still reads the larger of the last two
/// (a fall is believed after two windows) and refuses, and the one after reads 87 000 m and
/// places the lease that waited. Catches a saturated reading that holds the host after its
/// queue drained, and one forgotten a window early.
#[test]
fn a_full_host_takes_work_again_two_windows_after_its_queue_drains() {
    let (mut r, held) = saturated(87_000, 4_750_000);
    assert!(!ask(&mut r).0);
    let pressure = SINCE_BOOT_USEC + 4_750_000;
    window(&mut r, 87_000, pressure, &held);
    assert!(
        r.tick().is_empty(),
        "the newer of the two windows is still full"
    );
    window(&mut r, 87_000, pressure, &held);
    assert_eq!(
        r.s.load_view(&w("a")).unwrap().measured_cpu_millis,
        Some(87_000)
    );
    assert_eq!(r.tick().len(), 1);
}
