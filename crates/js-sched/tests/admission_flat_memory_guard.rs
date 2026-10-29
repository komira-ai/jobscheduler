//! The node guard on flat memory holds for every admission that adds memory: the sum of
//! the flat memory costs of the leases on a worker, with the new lease's, stays within
//! `(1 + d) * C_mem`, whether the new lease is charged by a curve or flat (a retry up the
//! memory ladder, a cold key, a key with no curve), on a worker charged by measurement or
//! by prediction alone.

#[path = "profile_rig/mod.rs"]
mod profile_rig;

use js_sched::LoadConfig;
use js_types::sched::{Failure, LeaseKind, Outcome, Resources, StartLease};

use profile_rig::load_curves::Curves;
use profile_rig::load_rig::{self, GIB, Rig, cold, warm};
use profile_rig::{HOUR, cluster_on, flat_ws, place};

/// The worker of these tests: 32 GiB, memory slack of the predicate 0.
fn small() -> Resources {
    Resources::new(100_000, 32 * GIB)
}

/// A worker `a` with 32 GiB under `config` and curves of 1 GiB for the keys `o` and `k`.
fn rig(config: LoadConfig) -> Rig {
    let (r, curves) = cluster_on(config, small());
    for key in ["o", "k"] {
        curves.set(key, &flat_ws(&[1_000], GIB), 5);
    }
    r
}

/// Runs `key`, killed at its limit, and the retry that follows: its start if it was
/// placed (charged flat at the 16 GiB rung).
fn killed_and_retried(r: &mut Rig, key: &str) -> Option<StartLease> {
    let first = place(r, key, warm(1_000, 2 * GIB));
    r.end(&first, Outcome::Failed(Failure::OutOfMemory));
    r.tick().into_iter().next()
}

fn starts(r: &mut Rig) -> usize {
    r.tick().len()
}

/// Catches: the guard skipped for a lease charged flat. `o` books 20 GiB under a curve of
/// 1 GiB on a 32 GiB worker, and `k`, killed at its limit, retries at the 16 GiB rung,
/// charged flat: 20 + 16 is over 32, so the retry waits, and runs when `o` has ended.
#[test]
fn a_flat_retry_beside_a_curved_lease_waits_for_it() {
    let mut r = rig(LoadConfig::default());
    let o = place(&mut r, "o", warm(1_000, 20 * GIB));
    assert!(killed_and_retried(&mut r, "k").is_none(), "20 + 16 > 32");
    assert_eq!(starts(&mut r), 0, "and it keeps waiting");
    r.finish(&o);
    let retry = r.tick().remove(0);
    let cost = r.s.lease_cost(retry.lease).expect("held");
    assert_eq!(cost.memory_bytes, 16 * GIB, "the retry, at its rung");
}

/// Catches: the outcome depending on which of the two leases came first: the retry runs
/// first, and the curved lease booking 20 GiB then waits for it.
#[test]
fn the_same_pair_in_the_other_order_gives_the_same_outcome() {
    let mut r = rig(LoadConfig::default());
    let retry = killed_and_retried(&mut r, "k").expect("alone on the worker");
    r.keyed("o", 1_000, 8 * GIB, warm(1_000, 20 * GIB));
    assert_eq!(starts(&mut r), 0, "20 + 16 > 32");
    r.finish(&retry);
    assert_eq!(starts(&mut r), 1, "runs when the retry has ended");
}

/// Catches: a cold key, charged flat, outside the guard: a cold lease of 16 GiB beside a
/// curved lease booking 20 GiB is refused, and one of 12 GiB (32 in all) is admitted.
#[test]
fn a_cold_key_that_would_pass_the_flat_sum_is_refused() {
    let mut r = rig(LoadConfig::default());
    place(&mut r, "o", warm(1_000, 20 * GIB));
    r.keyed("c", 1_000, 8 * GIB, cold(1_000, 16 * GIB));
    assert_eq!(starts(&mut r), 0, "20 + 16 > 32");
    let mut r = rig(LoadConfig::default());
    place(&mut r, "o", warm(1_000, 20 * GIB));
    r.keyed("c", 1_000, 8 * GIB, cold(1_000, 12 * GIB));
    assert_eq!(starts(&mut r), 1, "20 + 12 = 32");
}

/// Catches: the guard held only where memory is charged by curve: with no curve at all and
/// the predicate's own slack at 50% (a limit of 48 GiB), a flat lease of 16 GiB beside one
/// of 20 is refused by the guard (limit 32) and one of 12 GiB is admitted; with the guard
/// off the predicate alone decides and both are admitted.
#[test]
fn a_flat_charged_pair_is_held_to_the_flat_sum() {
    let second = |slack: Option<u32>, bytes: u64| {
        let config = LoadConfig {
            s_mem_permille: 500,
            flat_memory_slack_permille: slack,
            ..LoadConfig::default()
        };
        let (mut r, _) = cluster_on(config, small());
        r.keyed("p", 1_000, 8 * GIB, warm(1_000, 20 * GIB));
        assert_eq!(starts(&mut r), 1);
        r.keyed("q", 1_000, 8 * GIB, warm(1_000, bytes));
        starts(&mut r)
    };
    assert_eq!(second(Some(0), 12 * GIB), 1, "exactly the worker's memory");
    assert_eq!(second(Some(0), 12 * GIB + 1), 0, "a byte over");
    assert_eq!(second(Some(250), 16 * GIB), 1, "within 40 GiB");
    assert_eq!(second(None, 16 * GIB), 1);
}

/// Catches: the guard left out on a worker that never sent a reading (charged by
/// prediction alone), where it is stricter than the predicate only when its slack is
/// below the predicate's: with the predicate at 50% a flat lease of 16 GiB beside one of
/// 20 is refused by the guard at `d` = 0, and admitted with the guard off.
#[test]
fn a_worker_charged_by_prediction_alone_is_held_to_the_flat_sum() {
    let second = |slack: Option<u32>| {
        let config = LoadConfig {
            s_mem_permille: 500,
            flat_memory_slack_permille: slack,
            ..LoadConfig::default()
        };
        let mut r = Rig::with_curves(config, Curves::new("load"));
        r.up("a", small());
        r.set_clock("a", HOUR);
        r.keyed("p", 1_000, 8 * GIB, warm(1_000, 20 * GIB));
        assert_eq!(starts(&mut r), 1);
        r.keyed("q", 1_000, 8 * GIB, warm(1_000, 16 * GIB));
        starts(&mut r)
    };
    assert_eq!(second(Some(0)), 0);
    assert_eq!(second(None), 1);
}

/// Catches: the setting off still refusing: with `None` the retry beside the curved
/// lease and the cold lease beside it are both admitted, as the curves and the flat rule
/// alone decide.
#[test]
fn with_the_guard_off_both_are_admitted() {
    let off = LoadConfig {
        flat_memory_slack_permille: None,
        ..LoadConfig::default()
    };
    let mut r = rig(off);
    place(&mut r, "o", warm(1_000, 20 * GIB));
    assert!(killed_and_retried(&mut r, "k").is_some());
    let mut r = rig(off);
    place(&mut r, "o", warm(1_000, 20 * GIB));
    r.keyed("c", 1_000, 8 * GIB, cold(1_000, 16 * GIB));
    assert_eq!(starts(&mut r), 1);
}

/// Catches: a lease that owns the worker whole held to the guard: a whole-machine lease
/// and an action at the whole host's memory are admitted on an idle worker with the guard
/// at `d` = 0, and neither is placed beside another lease.
#[test]
fn whole_machine_and_exclusive_leases_keep_their_rules() {
    let mut r = Rig::with_curves(LoadConfig::default(), Curves::new("load"));
    r.up_with(
        "a",
        small(),
        load_rig::caps("container").with_drivers(["container", "native-whole-machine"]),
    );
    r.set_clock("a", HOUR);
    r.baseline("a");
    r.window("a", 0, 0, &[]);
    let whole = r.request(LeaseKind::WholeMachine, 1_000, 32 * GIB);
    r.submit(whole);
    assert_eq!(starts(&mut r), 1);
    let mut r = rig(LoadConfig::default());
    r.keyed("x", 1_000, 32 * GIB, warm(1_000, 32 * GIB));
    assert_eq!(starts(&mut r), 1);
    r.keyed("y", 1_000, GIB, warm(1_000, GIB));
    assert_eq!(starts(&mut r), 0, "nothing beside it");
}

/// A measured worker `a` of 32 GiB with one VM slot, which runs VMs.
fn vm_worker(config: LoadConfig) -> Rig {
    let mut r = Rig::with_curves(config, Curves::new("load"));
    r.up_with(
        "a",
        small().with_vms(1),
        load_rig::caps("container").with_drivers(["container", "vm"]),
    );
    r.set_clock("a", HOUR);
    r.baseline("a");
    r.window("a", 0, 0, &[]);
    r
}

/// A worker with one VM slot holding an action that books 8 GiB and is charged `flat`
/// bytes flat (a retry up the ladder is charged above its booking), then a VM of
/// `vm_bytes`: how many leases start for the VM, 0 or 1.
fn vm_beside_an_action(slack: Option<u32>, flat: u64, vm_bytes: u64) -> usize {
    let config = LoadConfig {
        flat_memory_slack_permille: slack,
        ..LoadConfig::default()
    };
    let mut r = vm_worker(config);
    r.keyed("p", 1_000, 8 * GIB, warm(1_000, flat));
    assert_eq!(starts(&mut r), 1, "the action is placed");
    let mut vm = r.request(LeaseKind::Vm, 4_000, vm_bytes);
    vm.resources = vm.resources.with_vms(1);
    r.submit(vm);
    starts(&mut r)
}

/// Catches: a VM judged by bookings alone. The action books 8 GiB but is charged 20 GiB
/// flat; the VM books 16 GiB, which fits the 24 GiB still unbooked, but 20 + 16 is over
/// the worker's 32 GiB, so the VM waits.
#[test]
fn a_vm_beside_actions_whose_flat_costs_exceed_their_bookings_waits() {
    assert_eq!(vm_beside_an_action(Some(0), 20 * GIB, 16 * GIB), 0);
}

/// Catches: the VM's memory off by a byte in the flat sum, or the sum compared with `>=`:
/// a VM of 12 GiB beside the 20 GiB action makes exactly 32 GiB and is admitted, one
/// byte more is refused (both fit the unbooked 24 GiB).
#[test]
fn a_vm_fits_the_flat_sum_exactly_and_a_byte_more_is_refused() {
    assert_eq!(vm_beside_an_action(Some(0), 20 * GIB, 12 * GIB), 1);
    assert_eq!(vm_beside_an_action(Some(0), 20 * GIB, 12 * GIB + 1), 0);
}

/// Catches: the guard off still refusing a VM: with `None` the VM is placed by the
/// booked-capacity rule alone, as before.
#[test]
fn with_the_guard_off_a_vm_is_admitted_by_its_booking_alone() {
    assert_eq!(vm_beside_an_action(None, 20 * GIB, 16 * GIB), 1);
}

/// Catches: the guard applied to a VM that owns the worker: a VM booking the worker's
/// whole memory is placed on an idle worker, where nothing is summed beside it.
#[test]
fn a_vm_at_the_whole_host_memory_on_an_idle_worker_is_admitted() {
    let mut r = vm_worker(LoadConfig::default());
    let mut vm = r.request(LeaseKind::Vm, 4_000, 32 * GIB);
    vm.resources = vm.resources.with_vms(1);
    r.submit(vm);
    assert_eq!(starts(&mut r), 1);
}
