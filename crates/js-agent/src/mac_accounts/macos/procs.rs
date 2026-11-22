//! Processes on macOS: the live processes of a uid through `libproc`, and the kill of them
//! all.

use std::collections::BTreeSet;
use std::io;

use super::super::host::LiveProcess;

/// `libproc`'s filters (`sys/proc_info.h`): processes by effective and by real uid.
const PROC_UID_ONLY: u32 = 4;
const PROC_RUID_ONLY: u32 = 5;
/// `pbi_status` of a process that has exited and waits to be reaped.
const SZOMB: u32 = 5;

/// The live processes whose effective or real uid is `uid`.
pub(super) fn live_processes(uid: u32) -> io::Result<Vec<LiveProcess>> {
    let mut pids = BTreeSet::new();
    for filter in [PROC_UID_ONLY, PROC_RUID_ONLY] {
        pids.extend(list_pids(filter, uid)?);
    }
    Ok(pids.into_iter().filter_map(live).collect())
}

/// The pids `libproc` lists for `filter` (by effective or real uid) and `uid`.
fn list_pids(filter: u32, uid: u32) -> io::Result<Vec<libc::pid_t>> {
    // SAFETY: a null buffer asks only for the size needed.
    let bytes = unsafe { libc::proc_listpids(filter, uid, std::ptr::null_mut(), 0) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for some more pids than the kernel reported, should some start meanwhile.
    let size = std::mem::size_of::<libc::pid_t>();
    let mut pids = vec![0 as libc::pid_t; bytes.unsigned_abs() as usize / size + 64];
    let room = i32::try_from(pids.len() * size).map_err(io::Error::other)?;
    // SAFETY: the buffer holds `room` bytes of pids and outlives the call.
    let wrote = unsafe { libc::proc_listpids(filter, uid, pids.as_mut_ptr().cast(), room) };
    if wrote < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(wrote.unsigned_abs() as usize / size);
    pids.retain(|&pid| pid > 0);
    Ok(pids)
}

/// `pid` with its command name, unless it has exited: it waits to be reaped, or is gone
/// already.
fn live(pid: libc::pid_t) -> Option<LiveProcess> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    let error = (wrote <= 0).then(io::Error::last_os_error);
    // SAFETY: read only when the call filled the whole struct.
    let info = (wrote == size).then(|| unsafe { info.assume_init() });
    if exited_from(info.map(|info| info.pbi_status), error.as_ref()) {
        return None;
    }
    let command = info.map_or_else(|| "?".to_owned(), |info| c_name(&info.pbi_comm));
    Some(LiveProcess { pid, command })
}

/// A NUL-terminated name of at most `raw.len()` bytes, as text.
fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What `proc_pidinfo` said, as [`live`] decides it: a full answer is a zombie or a live
/// process by its status; otherwise only "no such process" (`ESRCH`) is gone. Any other
/// failure, or a short answer, counts the process as live, so that a deletion refuses
/// rather than under-counts.
fn exited_from(status: Option<u32>, error: Option<&io::Error>) -> bool {
    match status {
        Some(status) => status == SZOMB,
        None => error.and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
    }
}

/// Sends SIGKILL to every process of `uid`: a child that becomes the uid and calls `kill(-1)`,
/// which signals every process the uid may signal, in one call the uid's processes cannot
/// outrun by forking.
pub(super) fn kill_all(uid: u32) -> io::Result<()> {
    // SAFETY: the child calls only async-signal-safe functions (setuid, kill, _exit) before
    // it exits, as a fork of a threaded process must.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // As the uid, kill(-1) signals every process the uid may signal (those whose real or
        // saved uid it is). On macOS the sender is one of them, so this child usually ends
        // here by SIGKILL; elsewhere the loop ends with ESRCH.
        // SAFETY: async-signal-safe calls only; `_exit` never returns.
        unsafe {
            if libc::setuid(uid) != 0 {
                libc::_exit(1);
            }
            while libc::kill(-1, libc::SIGKILL) == 0 {}
            libc::_exit(0);
        }
    }
    let mut status = 0;
    // SAFETY: `pid` is this process's child; `status` is writable.
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // macOS's kill(-1) signals the sender too, so the killer ending by SIGKILL is success:
    // the signals to every other process of the uid were posted in the same call. An exit
    // of 1 is a failed setuid.
    let killed_itself = libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL;
    let exited_clean = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
    if !killed_itself && !exited_clean {
        return Err(io::Error::other(format!(
            "the killer for uid {uid} failed to drop to it ({status:#x})"
        )));
    }
    Ok(())
}
