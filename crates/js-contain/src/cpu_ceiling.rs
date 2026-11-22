//! An optional CPU ceiling for a lease ([`CpuCeiling`], defined in `js-types` so that every
//! runtime of the agent can name it), and the `cpu.max` text it is written as ([`cpu_max`]).
//!
//! A lease's CPU booking is what placement packs a host by and what the lease is guaranteed:
//! its `cpu.weight` keeps that guarantee when the host is busier than planned, and nothing
//! holds a job below what it needs. With no ceiling (the default) a job also uses every core
//! no other job wants. A ceiling, when the operator sets one, is a `cpu.max` of the booking
//! times the multiple on the lease and on the container's own cgroup, so a job is held to it
//! and reads it as its CPU count; at a multiple of 1 a job is held to its booking.

pub use js_types::{CpuCeiling, CpuCeilingError};

/// The CFS period of `cpu.max`, in microseconds: the kernel's default.
const CPU_PERIOD: u64 = 100_000;
/// The least quota the kernel takes, in microseconds.
const CPU_QUOTA_FLOOR: u64 = 1000;
/// The `cpu.max` text of a ceiling of `cpu_millis` thousandths of a core: the cores as a quota
/// per 100 ms period (1.5 cores is `150000 100000`), never below the kernel's least quota.
/// `None` for 0.
#[must_use]
pub fn cpu_max(cpu_millis: u64) -> Option<String> {
    (cpu_millis > 0).then(|| {
        let quota = cpu_millis
            .saturating_mul(CPU_PERIOD / 1000)
            .max(CPU_QUOTA_FLOOR);
        format!("{quota} {CPU_PERIOD}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a `cpu.max` that is not the cores as a quota per 100 ms period, one below the
    /// kernel's least quota, and one written for 0.
    #[test]
    fn a_ceiling_is_the_cores_as_a_quota_per_period() {
        assert_eq!(cpu_max(1500).as_deref(), Some("150000 100000"));
        assert_eq!(cpu_max(1).as_deref(), Some("1000 100000"));
        assert_eq!(cpu_max(0), None);
    }
}
