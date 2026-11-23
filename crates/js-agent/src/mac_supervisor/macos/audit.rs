//! Audit sessions: a new one for the supervisor (and so the job, which inherits it), and the
//! session of any process, by pid, for the sweep that ends a job's processes.

use std::ffi::{c_int, c_void};
use std::io;
use std::mem;

/// `au_mask_t` of `<bsm/audit.h>`.
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
#[derive(Debug, Default, Clone, Copy)]
struct AuMask {
    am_success: u32,
    am_failure: u32,
}

/// `au_tid_addr_t` of `<bsm/audit.h>` (`at_port` is a `dev_t`, 32 bits on macOS).
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
#[derive(Debug, Default, Clone, Copy)]
struct AuTidAddr {
    at_port: i32,
    at_type: u32,
    at_addr: [u32; 4],
}

/// `auditinfo_addr_t` of `<bsm/audit.h>`.
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
#[derive(Debug, Default, Clone, Copy)]
struct AuditinfoAddr {
    ai_auid: u32,
    ai_mask: AuMask,
    ai_termid: AuTidAddr,
    ai_asid: i32,
    ai_flags: u64,
}

/// `auditpinfo_addr_t` of `<bsm/audit.h>`.
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
#[derive(Debug, Default, Clone, Copy)]
struct AuditpinfoAddr {
    ap_pid: i32,
    ap_auid: u32,
    ap_mask: AuMask,
    ap_termid: AuTidAddr,
    ap_asid: i32,
    ap_flags: u64,
}

// The sizes `<bsm/audit.h>` gives on macOS 27 (arm64): 48 and 56 bytes.
const _: () = assert!(mem::size_of::<AuditinfoAddr>() == 48);
const _: () = assert!(mem::size_of::<AuditpinfoAddr>() == 56);

/// `A_GETPINFO_ADDR` of `<bsm/audit.h>`: a process's audit information, by pid.
const A_GETPINFO_ADDR: c_int = 28;

/// `AU_ASSIGN_ASID` of `<bsm/audit.h>`: the kernel picks a new, unused session id.
const AU_ASSIGN_ASID: i32 = -1;

// FFI-BOUNDARY: each audit call reads or writes the one struct it is given, owned by the caller
// and of the length given, and keeps no pointer to it after it returns. Nothing is allocated on
// either side, so nothing is freed.
unsafe extern "C" {
    fn getaudit_addr(info: *mut AuditinfoAddr, length: c_int) -> c_int;
    fn setaudit_addr(info: *const AuditinfoAddr, length: c_int) -> c_int;
    fn auditon(cmd: c_int, data: *mut c_void, length: c_int) -> c_int;
}

/// The size of `T` as the C calls take it.
fn c_len<T>() -> c_int {
    // The structs here are tens of bytes.
    c_int::try_from(mem::size_of::<T>()).unwrap_or(c_int::MAX)
}

/// Opens a new audit session for this process, with `auid` as its audit user: its id. Only
/// root may. The processes this one starts after are in the session too.
pub(crate) fn new_audit_session(auid: u32) -> io::Result<i32> {
    let mut info = AuditinfoAddr::default();
    // SAFETY: `info` is ours, alive for the call and of the length given; the call writes it
    // and keeps no pointer (see FFI-BOUNDARY).
    if unsafe { getaudit_addr(&mut info, c_len::<AuditinfoAddr>()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    info.ai_asid = AU_ASSIGN_ASID;
    info.ai_auid = auid;
    // SAFETY: as above; the call only reads `info`.
    if unsafe { setaudit_addr(&info, c_len::<AuditinfoAddr>()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as the first call.
    if unsafe { getaudit_addr(&mut info, c_len::<AuditinfoAddr>()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ai_asid)
}

/// The audit session of process `pid`, or `None` when it cannot be read (there is no such
/// process, or this one may not ask).
#[must_use]
pub fn asid_of(pid: i32) -> Option<i32> {
    let mut info = AuditpinfoAddr {
        ap_pid: pid,
        ..AuditpinfoAddr::default()
    };
    // SAFETY: `info` is ours, alive for the call and of the length given; `auditon` reads
    // `ap_pid`, writes the rest of the struct, and keeps no pointer (see FFI-BOUNDARY).
    let rc = unsafe {
        auditon(
            A_GETPINFO_ADDR,
            (&raw mut info).cast(),
            c_len::<AuditpinfoAddr>(),
        )
    };
    (rc == 0).then_some(info.ap_asid)
}
