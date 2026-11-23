//! The usage of a uid's processes on macOS: each pid of the uid (`libproc`'s listing by
//! effective and by real uid, [`crate::mac_accounts`]) read by `proc_pid_rusage`, whose times
//! are ticks of the Mac's absolute time (`mach_timebase_info`).
//!
//! # FFI-BOUNDARY
//!
//! `proc_pid_rusage` and `mach_timebase_info` write a struct the caller owns, here a local of
//! the call, and keep no pointer after they return; nothing is allocated or freed across the
//! boundary.

use std::ffi::c_int;
use std::io;
use std::mem::MaybeUninit;

use super::{ProcUsage, ProcessUsage, ticks_to_usec};
use crate::mac_accounts::macos::procs::pids_of;

// The `libc` crate declares `mach_timebase_info` deprecated (for the `mach2` crate), which this
// crate does not depend on: the one call is declared here, as libSystem has it.
unsafe extern "C" {
    /// Fills the timebase: its numerator, then its denominator.
    fn mach_timebase_info(info: *mut [u32; 2]) -> c_int;
}

/// The processes of a uid as `libproc` reads them, with the timebase of this Mac.
#[derive(Debug, Clone, Copy)]
pub struct MacProcesses {
    /// The timebase's numerator and denominator: ticks times one over the other are
    /// nanoseconds.
    timebase: (u32, u32),
}

impl MacProcesses {
    /// The processes of this Mac. A timebase that cannot be read (which the system never
    /// refuses) is taken as 1/1, the ratio of an Intel Mac.
    #[must_use]
    pub fn new() -> Self {
        let mut info = [0u32; 2];
        // SAFETY: `info` is the two words of a `mach_timebase_info_data_t`, which the call
        // fills and keeps no pointer to.
        let status = unsafe { mach_timebase_info(&raw mut info) };
        let timebase = match status {
            0 => (info[0], info[1]),
            _ => (1, 1),
        };
        Self { timebase }
    }

    /// What `pid` has used, or `None` when it cannot be read (it has ended, or is not there).
    fn usage_of(&self, pid: libc::pid_t) -> Option<ProcUsage> {
        let mut info = MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: `info` is a writable `rusage_info_v2`, the struct the flavor
        // `RUSAGE_INFO_V2` fills; the call keeps no pointer to it.
        let status =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) };
        if status != 0 {
            return None;
        }
        // SAFETY: the call returned 0, so it filled the whole struct (and all zeroes, which
        // it started as, is a valid value of a struct of integers).
        let info = unsafe { info.assume_init() };
        let ticks = [
            info.ri_user_time,
            info.ri_system_time,
            info.ri_child_user_time,
            info.ri_child_system_time,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);
        let (numer, denom) = self.timebase;
        Some(ProcUsage {
            cpu_usec: ticks_to_usec(ticks, numer, denom),
            footprint_bytes: info.ri_phys_footprint,
        })
    }
}

impl Default for MacProcesses {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessUsage for MacProcesses {
    fn of_uid(&self, uid: u32) -> io::Result<Vec<ProcUsage>> {
        Ok(pids_of(uid)?
            .into_iter()
            .filter_map(|pid| self.usage_of(pid))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{MacProcesses, ProcessUsage as _};

    /// Asserts, on a Mac, that the processes of the test's own uid are read, this test's
    /// process among them: some process of the uid holds memory and has used CPU. Catches a
    /// listing that finds none of the uid's processes, and a footprint or a CPU read from the
    /// wrong field of the struct or in the wrong unit (all zero).
    #[test]
    fn the_processes_of_a_uid_are_read_with_their_cpu_and_footprint() {
        let uid = rustix::process::geteuid().as_raw();
        let processes = MacProcesses::new().of_uid(uid).unwrap();
        assert!(!processes.is_empty());
        assert!(
            processes.iter().any(|p| p.footprint_bytes > 0),
            "{processes:?}"
        );
        assert!(processes.iter().any(|p| p.cpu_usec > 0), "{processes:?}");
    }
}
