//! The counters of a Mac's host, for its load ([`super::HostSampler`]): the CPU ticks of every
//! CPU state summed over the CPUs (`host_statistics`, `HOST_CPU_LOAD_INFO`) and the VM
//! statistics (`host_statistics64`, `HOST_VM_INFO64`), whose anonymous, wired and compressed
//! pages are the non-file memory ([`VmPages`]).
//!
//! # FFI-BOUNDARY
//!
//! `host_statistics` and `host_statistics64` write a struct the caller owns (a local of the
//! call) and the count of its words, and keep no pointer after they return. `mach_host_self`
//! returns the host's name port, which is not freed: it is taken once, when [`MacHost`] is
//! made, and held for the agent's life.

use std::mem::MaybeUninit;

use super::host_sampler::{CpuTicks, HostStatistics, VmPages};

// The `libc` crate declares `mach_host_self` deprecated (for the `mach2` crate), which this
// crate does not depend on: the one call is declared here, as libSystem has it.
unsafe extern "C" {
    /// The name port of this host.
    fn mach_host_self() -> libc::mach_port_t;
}

/// The ticks a second of the CPU counters when the system does not say (`CLK_TCK` of macOS).
const DEFAULT_TICKS_PER_SECOND: u64 = 100;

/// The host of this Mac, as its counters read.
#[derive(Debug, Clone, Copy)]
pub struct MacHost {
    /// The host's name port.
    host: libc::mach_port_t,
    /// The bytes of a page of the VM statistics.
    page_bytes: u64,
    /// The ticks a second of the CPU counters.
    ticks_per_second: u64,
}

impl MacHost {
    /// The host of this Mac: its name port, its page size and its clock ticks a second.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: the call takes nothing and returns the host's name port.
        let host = unsafe { mach_host_self() };
        // SAFETY: `vm_page_size` is a constant the system sets before any program runs.
        let page = unsafe { libc::vm_page_size };
        // SAFETY: `sysconf` takes an integer and reads no memory of the caller's.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        Self {
            host,
            page_bytes: u64::try_from(page).unwrap_or(0),
            ticks_per_second: u64::try_from(ticks)
                .ok()
                .filter(|&ticks| ticks > 0)
                .unwrap_or(DEFAULT_TICKS_PER_SECOND),
        }
    }
}

impl Default for MacHost {
    fn default() -> Self {
        Self::new()
    }
}

impl HostStatistics for MacHost {
    fn cpu_ticks(&self) -> Option<CpuTicks> {
        let mut info = MaybeUninit::<libc::host_cpu_load_info>::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: `info` is a writable `host_cpu_load_info` of `count` words, the struct the
        // flavor `HOST_CPU_LOAD_INFO` fills; the call keeps no pointer to it or to `count`.
        let status = unsafe {
            libc::host_statistics(
                self.host,
                libc::HOST_CPU_LOAD_INFO,
                info.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        if status != libc::KERN_SUCCESS {
            return None;
        }
        // SAFETY: the call succeeded, so it filled the struct (which started all zeroes, a
        // valid value of an array of integers).
        let ticks = unsafe { info.assume_init() }.cpu_ticks;
        let at = |state: libc::c_int| ticks[usize::try_from(state).unwrap_or(0)];
        Some(CpuTicks {
            user: at(libc::CPU_STATE_USER),
            system: at(libc::CPU_STATE_SYSTEM),
            idle: at(libc::CPU_STATE_IDLE),
            nice: at(libc::CPU_STATE_NICE),
        })
    }

    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    fn nonfile_bytes(&self) -> Option<u64> {
        let mut info = MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `info` is a writable `vm_statistics64` of `count` words, the struct the
        // flavor `HOST_VM_INFO64` fills (a kernel with a shorter one fills its part and says
        // so in `count`); the call keeps no pointer to it or to `count`.
        let status = unsafe {
            libc::host_statistics64(
                self.host,
                libc::HOST_VM_INFO64,
                info.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        if status != libc::KERN_SUCCESS {
            return None;
        }
        // SAFETY: the call succeeded, so it filled the struct (which started all zeroes, a
        // valid value of a struct of integers).
        let info = unsafe { info.assume_init() };
        // Each field is copied out of the packed struct, never borrowed.
        let pages = VmPages {
            internal: u64::from(info.internal_page_count),
            purgeable: u64::from(info.purgeable_count),
            wired: u64::from(info.wire_count),
            compressed: u64::from(info.compressor_page_count),
        };
        Some(pages.nonfile_bytes(self.page_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::{HostStatistics as _, MacHost};

    /// Asserts, on a Mac, that the host's counters read: the CPU has been busy since boot,
    /// two readings never go back, the clock ticks a second are macOS's 100, and the non-file
    /// memory is some memory and less than the Mac has. Catches a flavor or a count the
    /// system refuses (no reading at all), counters read from the wrong slots, and a page
    /// size that is not the statistics' (memory far above the Mac's).
    #[test]
    fn the_host_s_cpu_ticks_and_non_file_memory_are_read() {
        let host = MacHost::new();
        let first = host.cpu_ticks().unwrap();
        assert!(first.user > 0 && first.idle > 0, "{first:?}");
        let second = host.cpu_ticks().unwrap();
        assert!(
            second.user >= first.user && second.idle >= first.idle,
            "{second:?}"
        );
        assert_eq!(host.ticks_per_second(), 100);
        let nonfile = host.nonfile_bytes().unwrap();
        let mut total = 0u64;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: the name is NUL-terminated, `total` is a writable `u64` of `size` bytes (what
        // `hw.memsize` is), and nothing is set.
        let status = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut total).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(status, 0);
        assert!(nonfile > 0 && nonfile < total, "{nonfile} of {total}");
    }
}
