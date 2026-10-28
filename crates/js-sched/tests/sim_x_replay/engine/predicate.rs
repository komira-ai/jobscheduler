//! The admission predicate of the replay: a stand-in, one function for every ranker.
//!
//! It models a rule that charges a lease by a curve over its life. That rule exists on no
//! branch yet, so this is a stand-in and the report says so; the flat rule it extends is
//! pinned slot by slot in the tests (the same sums, the same caps, the same clamp of a
//! charge to the host).
//!
//! Slot 0 follows the flat rule's formulas. With `C` the host's capacity, `p` the flat
//! charge of a lease and `m` its memory:
//!
//! ```text
//! CPU:    max(measured, f * sum p(covered)) + sum p(uncovered) + p_new  <=  C_cpu * (1 + s_cpu)
//! memory: max(sum m, working_set + sum m(uncovered)) + m_new            <=  C_mem * (1 + s_mem)
//! ```
//!
//! Slots 1 to 30 and `H+` add the residents' charges that many minutes ahead (each at its
//! own age) to the new lease's charge, against the same caps. A lease cap bounds the
//! residents, and a cold lease is admitted only while the host's cold count of the round
//! is under the cold cap (`ceil(C_cpu / 32 cores)`).
//!
//! What the stand-in leaves out of the merged rule, so a result is read with it in mind:
//!
//! * no VM term (`V_cpu`, `V_mem`) for a VM lease;
//! * no fallback for a node that has reported nothing, and no 1000 m minimum charge;
//! * no `working_set_counted` gate: the working set always counts;
//! * the cold cap per window is fixed at `ceil(C_cpu / 32 cores)`, not configurable;
//! * no exclusive-worker case, which the merged rule admits without scoring;
//! * no hold of the head of the queue on the host with the most headroom while it is
//!   blocked.
//!
//! The order of the checks fixes which refusal is reported: the lease cap, slot 0 (CPU,
//! then memory), the later slots in order, and the cold cap last, so a cold cap is
//! reported only when it is the only obstacle (as the flat rule reports it).

use super::model::{Capacity, Charge, HostState, SLOTS};

/// The predicate's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// `f`, in thousandths: covered leases are charged at least this share of their flat
    /// cores.
    pub f_permille: u64,
    /// `s_cpu`, in thousandths.
    pub s_cpu_permille: u64,
    /// `s_mem`, in thousandths.
    pub s_mem_permille: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            f_permille: 750,
            s_cpu_permille: 100,
            s_mem_permille: 0,
        }
    }
}

impl Params {
    /// `C_cpu * (1 + s_cpu)`.
    pub fn cpu_limit(&self, cap: &Capacity) -> u64 {
        scale(cap.cpu_millis, 1_000 + self.s_cpu_permille)
    }

    /// `C_mem * (1 + s_mem)`.
    pub fn mem_limit(&self, cap: &Capacity) -> u64 {
        scale(cap.mem_mib, 1_000 + self.s_mem_permille)
    }

    /// `f * cores`, rounded up.
    pub fn floor_of(&self, cpu_millis: u64) -> u64 {
        (u128::from(cpu_millis) * u128::from(self.f_permille))
            .div_ceil(1_000)
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

fn scale(value: u64, permille: u64) -> u64 {
    u64::try_from(u128::from(value) * u128::from(permille) / 1_000).unwrap_or(u64::MAX)
}

/// Cold leases admitted per host per window: one per 32 cores, rounded up, at least 1.
pub fn cold_cap(cap: &Capacity) -> u32 {
    u32::try_from(cap.cpu_millis.div_ceil(32_000))
        .unwrap_or(u32::MAX)
        .max(1)
}

/// Why a host did not admit a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Refusal {
    /// The host holds as many leases as it may.
    LeaseCap,
    /// CPU, at this slot.
    Cpu {
        /// 0 is now, 1 to 30 minutes ahead, 31 the held tail.
        slot: usize,
    },
    /// Memory, at this slot.
    Memory {
        /// As for [`Refusal::Cpu`].
        slot: usize,
    },
    /// Only the cold cap of the round stands in the way.
    ColdCap,
}

/// The left side of slot 0: what the host is charged before a new lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Left {
    /// `max(measured, f * sum p(covered)) + sum p(uncovered)`, millicores.
    pub cpu_millis: u64,
    /// `max(sum m, working_set + sum m(uncovered))`, MiB.
    pub mem_mib: u64,
}

/// The left side of slot 0 of `host`.
pub fn left(params: &Params, host: &HostState<'_>) -> Left {
    let sums = host.sums();
    let cpu = host
        .measured_cpu_millis()
        .max(params.floor_of(sums.covered.cpu_millis))
        + sums.uncovered.cpu_millis;
    let mem = sums.slots[0]
        .mem_mib
        .max(host.working_set_mib() + sums.uncovered.mem_mib);
    Left {
        cpu_millis: cpu,
        mem_mib: mem,
    }
}

/// Whether `host` admits a lease with the belief `new`, and if not why.
pub fn admit(
    params: &Params,
    host: &HostState<'_>,
    new: &super::model::Belief,
) -> Result<(), Refusal> {
    let cap = &host.cap;
    if host.residents().len() >= usize::try_from(cap.leases).unwrap_or(usize::MAX) {
        return Err(Refusal::LeaseCap);
    }
    let (cpu_limit, mem_limit) = (params.cpu_limit(cap), params.mem_limit(cap));
    let at_zero = left(params, host);
    let p_new: Charge = new.at_slot(0, 0).clamped(cap);
    if at_zero.cpu_millis.saturating_add(p_new.cpu_millis) > cpu_limit {
        return Err(Refusal::Cpu { slot: 0 });
    }
    if at_zero.mem_mib.saturating_add(p_new.mem_mib) > mem_limit {
        return Err(Refusal::Memory { slot: 0 });
    }
    for slot in 1..SLOTS {
        let sum = host.sums().slots[slot];
        let add = new.at_slot(0, slot).clamped(cap);
        if sum.cpu_millis + add.cpu_millis > cpu_limit {
            return Err(Refusal::Cpu { slot });
        }
        if sum.mem_mib + add.mem_mib > mem_limit {
            return Err(Refusal::Memory { slot });
        }
    }
    if new.cold && host.cold_this_round() >= cold_cap(cap) {
        return Err(Refusal::ColdCap);
    }
    Ok(())
}
