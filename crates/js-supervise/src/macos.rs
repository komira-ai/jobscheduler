//! The process identity on macOS: the process table as `proc_pidinfo(PROC_PIDTBSDINFO)` and
//! `sysctl(KERN_PROCARGS2)` show it. Neither rustix nor nix wraps the two calls, so they are
//! made through libc here, each in a function that takes and returns plain values.

use std::path::Path;

/// The state letter and the start time (microseconds since the Unix epoch) of the process
/// `pid`. `None` when there is no such process, it is a zombie, or the answer is short.
pub(crate) fn stat_of(pid: u32) -> Option<(char, u64)> {
    let info = bsd_info(pid)?;
    let start = info
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec);
    Some((super::state_letter(info.pbi_status), start))
}

/// Whether the process `pid` runs as the user `uid` (its effective user).
pub(crate) fn runs_as(pid: u32, uid: u32) -> bool {
    bsd_info(pid).is_some_and(|info| info.pbi_uid == uid)
}

/// Whether the command line of the process `pid` names `lease` as the lease.
pub(crate) fn supervises(pid: u32, lease: &Path) -> bool {
    argument_area(pid).is_some_and(|area| {
        super::arguments(&area).is_some_and(|args| super::names_lease(args, lease))
    })
}

/// `pid` as a process id the kernel takes: a positive `pid_t`. Pid 0, which macOS gives the
/// kernel's own task, names no process here, as it names none in `/proc` on Linux.
fn process_id(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// The `proc_bsdinfo` of the process `pid`. `None` when there is no such process, or the
/// answer is short. macOS gives none for a zombie (a child that has ended and is not
/// collected), so a zombie reads as a process that is gone.
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = process_id(pid)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a zeroed `proc_bsdinfo` of `size` bytes that this function owns, and
    // the call writes at most `size` bytes through the pointer and keeps no copy of it.
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
    Some(unsafe { info.assume_init() })
}

/// The argument area of the process `pid` (`sysctl(KERN_PROCARGS2)`; [`super::arguments`]
/// reads it). `None` when there is no such process, or the area cannot be read: another
/// user's process for a caller that is not root.
fn argument_area(pid: u32) -> Option<Vec<u8>> {
    let pid = process_id(pid)?;
    let mut size = argument_max()?;
    let mut area = vec![0_u8; size];
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let name_len = libc::c_uint::try_from(name.len()).ok()?;
    // SAFETY: `name` holds the `name_len` integers of the name. `area` is a buffer of `size`
    // bytes that this function owns and `size` says so: the call writes at most that many
    // bytes, sets `size` to how many it wrote, and keeps no pointer. It sets no new value
    // (a null pointer and 0).
    let done = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            name_len,
            area.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if done != 0 {
        return None;
    }
    area.truncate(size);
    Some(area)
}

/// The most bytes a process's arguments and environment take (`sysctl(KERN_ARGMAX)`), the
/// size of the area [`argument_area`] reads into.
fn argument_max() -> Option<usize> {
    let mut name = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let name_len = libc::c_uint::try_from(name.len()).ok()?;
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: `name` holds the `name_len` integers of the name; `value` is an integer of
    // `size` bytes that this function owns, and the call writes at most `size` bytes to it,
    // sets `size` and keeps no pointer. It sets no new value (a null pointer and 0).
    let done = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            name_len,
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if done != 0 {
        return None;
    }
    usize::try_from(value).ok()
}
