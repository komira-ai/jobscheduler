//! The predicate over the horizon: the flat rule's two inequalities evaluated at each
//! minute ahead, on the worker's predicted load curve.
//!
//! For `tau` in `0..=HORIZON`, and once more at the `H+` slot, with `F` the covered
//! leases' charge and `U` the uncovered leases' charge at that slot:
//!
//! ```text
//! CPU:    F(tau) + U(tau) + V_cpu + c_new(tau)                           <= C_cpu * (1 + s_cpu)
//!         F(0) = max(measured, f * sum p(covered flat) + sum c(covered by curve))
//! memory: max(sum R(tau, all), working_set + sum R(tau, uncovered)) + V_mem + R_new(tau)
//!                                                                         <= C_mem * (1 + s_mem)
//!         host_used + sum R(tau, uncovered) + V_mem + R_new(tau)          (reading has host_used)
//! ```
//!
//! With the whole host's memory in use in the fresh reading (`host_used`), the leases a reading
//! covers are inside it and only the others are charged beside it: no lease is charged its
//! peak once a reading has seen it run, so the peaks of leases that do not peak together are
//! not summed. Without it the first form holds.
//!
//! Memory is checked at `tau = 0` alone: every term of its left side falls (or stays) as
//! `tau` grows, since a lease is charged the remaining maximum of its curve, which never
//! rises with the minute, or a flat figure, or nothing after its expected life.
//!
//! The measurement enters at `tau = 0` only, and only raises the figure. At every later
//! slot only the curves count. With no lease charged over time the slot-0 inequalities are
//! the flat rule's, and every later slot reads the same figures or less, so the result is
//! the flat rule's.
//!
//! One further guard, on by default (`d` = 0, see [`flat_memory_refusal`]): the sum of
//! the *flat* memory costs of every lease on the worker (cold and demoted ones included),
//! with the new lease's, must stay within `(1 + d) * C_mem`, whatever way the new lease
//! is charged. It bounds what a memory misfit shared by many keys can do, which the
//! curves cannot see, and what a lease charged flat (a retry up the memory ladder, a cold
//! key) adds beside leases charged by curve. With `host_used` in the fresh reading the sum
//! is `host_used` plus the flat costs of the leases no reading covers yet: the measurement
//! sees every misfit.

use js_types::sched::Resources;
use js_types::sched::usage::LeaseCost;

use super::LoadConfig;
use super::ledger::{Left, Totals};
use super::profile::{Profile, SLOTS};
use crate::refusal::{Refusal, RefusalKind};

/// The CPU a worker is charged `slot` minutes ahead, before a new lease: the covered
/// leases (at slot 0 raised to the measurement), the uncovered, and the VMs' booked cores.
pub(crate) fn cpu_left(t: &Totals, slot: usize, measured: u64, cfg: &LoadConfig) -> u64 {
    let covered = t.cpu_covered_at(slot, cfg);
    let covered = if slot == 0 {
        covered.max(measured)
    } else {
        covered
    };
    covered
        .saturating_add(t.cpu_uncovered_at(slot))
        .saturating_add(t.vm_cpu)
}

/// The memory a worker is charged `slot` minutes ahead, before a new lease: the whole host's
/// memory in use when the fresh reading measured it (`host_used`), plus the leases no reading
/// covers yet; else the larger of every lease's charge and the counted working set plus the
/// uncovered leases'. The VMs' booked memory is added either way; a running VM's memory is in
/// `host_used` too, an over-count that can only refuse work.
pub(crate) fn mem_left(t: &Totals, slot: usize, working_set: u64, host_used: Option<u64>) -> u64 {
    let leases = match host_used {
        Some(used) => used.saturating_add(t.mem_uncovered_at(slot)),
        None => t
            .mem_all_at(slot)
            .max(working_set.saturating_add(t.mem_uncovered_at(slot))),
    };
    leases.saturating_add(t.vm_mem)
}

/// What a new lease of `cost` (already clamped to `capacity`), charged over time as
/// `profile` says, is charged `slot` minutes ahead on a worker charged as `left`, as
/// `(millicores, bytes)`: its curve's figure at the slot, clamped to the worker; the flat
/// charge at every slot where the lease is charged flat, and on a worker charged by
/// prediction alone, where the least cores a lease is charged there apply.
pub(crate) fn new_charge(
    cost: &LeaseCost,
    profile: Option<&Profile>,
    left: &Left,
    capacity: &Resources,
    slot: usize,
) -> (u64, u64) {
    match profile.filter(|_| !left.fallback) {
        Some(p) => (
            p.new_cpu(slot).min(capacity.cpu_millis),
            p.new_mem(slot).min(capacity.memory_bytes),
        ),
        None => (left.charged_cpu(cost), cost.memory_bytes),
    }
}

/// Why a lease of `cost`, charged over time as `profile` says (flat when `None`), does not
/// fit a worker of `capacity` charged as `left`, or `None` if it fits at every slot.
/// CPU is judged first, then memory. The flat-memory guard is [`flat_memory_refusal`].
pub(crate) fn refusal(
    t: &Totals,
    cost: &LeaseCost,
    profile: Option<&Profile>,
    left: &Left,
    capacity: &Resources,
    cfg: &LoadConfig,
) -> Option<Refusal> {
    let cpu_limit = cfg.cpu_limit(capacity);
    let mem_limit = cfg.mem_limit(capacity);
    let new_cpu = |slot: usize| new_charge(cost, profile, left, capacity, slot).0;
    let new_mem = new_charge(cost, profile, left, capacity, 0).1;
    for slot in 0..SLOTS {
        let at = cpu_left(t, slot, left.measured, cfg);
        let new = new_cpu(slot);
        if at.saturating_add(new) > cpu_limit {
            let refusal = Refusal::new(RefusalKind::Cpu, at, new, cpu_limit);
            // Only a host charged by measurement is judged over the horizon.
            return Some(refusal.measured(Some(left.measured)));
        }
    }
    // Memory is checked at minute 0 alone: a charge by curve is the remaining maximum
    // from the minute on, which never rises with the minute, a flat charge is the same at
    // every minute, and a charge that ends with the lease's life only falls, so the sum at
    // any later minute is no more than at minute 0 (the measurement enters at 0 too).
    let at = mem_left(t, 0, left.working_set, left.host_memory);
    if at.saturating_add(new_mem) > mem_limit {
        let refusal = Refusal::new(RefusalKind::PredictedMemory, at, new_mem, mem_limit);
        return Some(refusal.measured_memory(left.host_memory));
    }
    None
}

/// Why the flat-memory guard refuses a lease of `cost` (already clamped to `capacity`):
/// the flat memory costs of every lease on the worker, cold and demoted ones included,
/// and the VMs' booked memory, with the new lease's, are above `(1 + d) * C_mem`; on a
/// worker whose fresh reading measured the host's memory in use (`host_used`), that figure
/// stands for the leases the reading covers, and the refusal carries it. It
/// holds for every action lease, charged by curve or flat, on a worker charged by
/// measurement or by prediction alone; `None` when the guard is off. A VM is judged by
/// the same sum, with its booked memory as its cost, besides the booked-capacity rule
/// it keeps. Whole-machine leases are never judged, and an action or a VM at the whole
/// host's memory runs on a worker with no lease, so it passes on its own cost.
pub(crate) fn flat_memory_refusal(
    t: &Totals,
    cost: &LeaseCost,
    capacity: &Resources,
    host_used: Option<u64>,
    cfg: &LoadConfig,
) -> Option<Refusal> {
    let d = cfg.flat_memory_slack_permille?;
    let leases = host_used.map_or(t.mem_all(), |used| used.saturating_add(t.mem_uncovered));
    let held = leases.saturating_add(t.vm_mem);
    let limit = super::scale(capacity.memory_bytes, 1000 + u64::from(d), 1000);
    (held.saturating_add(cost.memory_bytes) > limit).then(|| {
        Refusal::new(RefusalKind::BookedMemory, held, cost.memory_bytes, limit)
            .measured_memory(host_used)
    })
}
