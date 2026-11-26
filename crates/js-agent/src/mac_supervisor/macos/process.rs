//! The start time of a process, from `proc_pidinfo(PROC_PIDTBSDINFO)`: with its pid, what names
//! a supervisor to a later process of the agent (`super::super::adopt`).

/// `pbi_status` of a process that has exited and waits to be reaped (`SZOMB`,
/// `<sys/proc.h>`).
const SZOMB: u32 = 5;

/// When the live process `pid` started, in microseconds since the Unix epoch (`pbi_start_tvsec`
/// and `pbi_start_tvusec`); `None` when there is no such process, it has exited and waits to be
/// reaped, or the answer is short. Pid 0, the kernel's own task, names no process here.
#[must_use]
pub fn start_of(pid: u32) -> Option<u64> {
    let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a zeroed `proc_bsdinfo` of `size` bytes that this function owns; the
    // call writes at most `size` bytes through the pointer and keeps no copy of it.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote != size {
        return None;
    }
    // SAFETY: the call filled the whole struct (checked above) over zeroed bytes, and every
    // field of `proc_bsdinfo` is a plain integer or byte array, valid for any bytes.
    let info = unsafe { info.assume_init() };
    (info.pbi_status != SZOMB).then(|| {
        info.pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec)
    })
}

#[cfg(test)]
mod tests {
    use super::start_of;

    /// Asserts that this process has a start time, read the same twice, at or before now, and
    /// that pid 0 and a pid no process has have none. Catches a time read from the wrong field
    /// or in the wrong unit (seconds would be some 1 000 000 times too small), and a gone
    /// process given one.
    #[test]
    fn a_live_process_has_one_start_time_and_no_other_has_one() {
        let me = std::process::id();
        let started = start_of(me).unwrap();
        assert_eq!(start_of(me), Some(started));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let now = u64::try_from(now.as_micros()).unwrap();
        assert!(started <= now, "{started} after {now}");
        assert!(started > now / 2, "{started} is not in microseconds");
        assert_eq!(start_of(0), None);
        assert_eq!(start_of(u32::MAX), None);
    }
}
