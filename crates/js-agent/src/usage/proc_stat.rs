//! The whole host's CPU busy time, from the kernel's `/proc/stat`.
//!
//! The first line, `cpu  user nice system idle iowait irq softirq steal guest guest_nice`,
//! sums every CPU of the host in clock ticks since boot. The host is busy for all of it but
//! `idle` and `iowait` (a CPU waiting on a disk runs nothing). `guest` and `guest_nice` are
//! already counted in `user` and `nice`, so they are not added again. `steal`, the time a
//! hypervisor ran something else on the host's CPUs, is counted as busy: the host could not
//! use it. Every process counts, whichever cgroup it is in: the jobs, the agent, the storage
//! service, the kernel's threads and its interrupt handling.

/// Where the kernel publishes the host's CPU times.
pub const PROC_STAT: &str = "/proc/stat";

/// Microseconds in one clock tick of `/proc/stat`. The kernel reports these times in
/// `USER_HZ`, which its user-space interface fixes at 100 per second on the Linux
/// platforms the agent is released for (x86_64 and arm64), whatever the kernel's own tick.
pub const USEC_PER_TICK: u64 = 10_000;

/// The busy time of every CPU of the host since boot, in microseconds, from the text of
/// `/proc/stat`: `user + nice + system + irq + softirq + steal` of the line `cpu`, times
/// [`USEC_PER_TICK`]. `None` when there is no such line, when it has fewer than those eight
/// numbers (a kernel older than 2.6.11), when one of them is not a plain number, or when the
/// sum does not fit a `u64` of microseconds.
#[must_use]
pub fn parse_host_busy_usec(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("cpu"))?;
    let mut ticks = [0_u64; 8];
    let mut numbers = line.split_whitespace().skip(1);
    for slot in &mut ticks {
        let token = numbers.next()?;
        if !token.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *slot = token.parse().ok()?;
    }
    let [user, nice, system, _idle, _iowait, irq, softirq, steal] = ticks;
    [user, nice, system, irq, softirq, steal]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)?
        .checked_mul(USEC_PER_TICK)
}
