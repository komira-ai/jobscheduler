//! Every live process of the Mac through `libproc`, with its effective and real uid, and the
//! kill of one.
//!
//! # FFI-BOUNDARY
//!
//! `proc_listallpids` and `proc_pidinfo` write into buffers this module owns (a `Vec` of pids,
//! a `MaybeUninit` struct), sized by the byte counts passed with them, and keep no pointer
//! after the call; nothing is allocated by the C side, so nothing is freed. `kill` takes plain
//! values. No pointer leaves this module.

use std::io;
use std::mem::{MaybeUninit, size_of};

use super::super::Proc;

/// The pids of every process of the Mac, as `proc_listallpids` gives them.
pub(crate) fn all_pids() -> io::Result<Vec<libc::pid_t>> {
    // SAFETY: a null buffer of size 0 asks only for the number of processes; nothing is
    // written.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for some more pids than the kernel counted, should some start meanwhile.
    let mut pids = vec![0 as libc::pid_t; count.unsigned_abs() as usize + 256];
    let room = i32::try_from(pids.len() * size_of::<libc::pid_t>()).map_err(io::Error::other)?;
    // SAFETY: `pids` is ours, `room` bytes long, and outlives the call, which writes at most
    // `room` bytes and returns how many pids it wrote.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), room) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    let got = got.unsigned_abs() as usize;
    if got >= pids.len() {
        // A full buffer may have left processes out: the listing is not trusted.
        return Err(io::Error::other(format!(
            "more than {} processes started while the pids were read",
            pids.len() - count.unsigned_abs() as usize
        )));
    }
    pids.truncate(got);
    Ok(pids)
}

/// Every live process: zombies and processes gone since the listing are left out, and so are
/// pids 0 and 1 (the kernel and launchd, never a sweep's target). `asid_of` reads a
/// process's audit session when `with_asid` is set.
///
/// The short BSD info is read (`PROC_PIDT_SHORTBSDINFO`), which the kernel gives for any
/// process without the caller being its user. A process whose info cannot be read for any
/// reason but "no such process" fails the whole listing: its uid is unknown, and a sweep
/// must not call itself clean over a process it could not look at.
pub(super) fn list(with_asid: bool, asid_of: fn(i32) -> Option<i32>) -> io::Result<Vec<Proc>> {
    let mut procs = Vec::new();
    for pid in all_pids()? {
        if pid <= 1 {
            continue;
        }
        let Some(info) = short_info(pid)? else {
            continue;
        };
        if info.pbsi_status == libc::SZOMB {
            continue;
        }
        procs.push(Proc {
            pid,
            uid: info.pbsi_uid,
            ruid: info.pbsi_ruid,
            asid: if with_asid { asid_of(pid) } else { None },
            name: c_name(&info.pbsi_comm),
        });
    }
    Ok(procs)
}

/// The short BSD info of `pid`, or `None` when it is gone.
pub(crate) fn short_info(pid: libc::pid_t) -> io::Result<Option<libc::proc_bsdshortinfo>> {
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_bsdshortinfo>()).map_err(io::Error::other)?;
    // SAFETY: `info` is a writable `proc_bsdshortinfo` of `size` bytes, ours for the call,
    // which writes at most that many bytes.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote == size {
        // SAFETY: the call filled the whole struct (and it was zeroed before, a valid value of
        // the plain C struct).
        return Ok(Some(unsafe { info.assume_init() }));
    }
    if wrote > 0 {
        return Err(io::Error::other(format!(
            "process {pid}: a short answer of {wrote} bytes"
        )));
    }
    let why = io::Error::last_os_error();
    match why.raw_os_error() {
        Some(libc::ESRCH) => Ok(None),
        _ => Err(io::Error::new(why.kind(), format!("process {pid}: {why}"))),
    }
}

/// A NUL-terminated name of at most `raw.len()` bytes, as text.
pub(crate) fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Sends `SIGKILL` to `pid`, unless it is below 2. A process already gone (`ESRCH`) is not an
/// error, and no other failure is reported either: the next listing shows whether it ended.
pub(super) fn kill(pid: i32) {
    // The sweep never targets a pid below 2; this keeps a `kill` of a group (a negative
    // pid), of every process (-1) or of launchd from ever being sent.
    if pid <= 1 {
        return;
    }
    // SAFETY: `kill` takes plain values and touches no memory of ours; `pid` is a single
    // process's.
    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
}
