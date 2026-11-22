//! The system calls of `__mac-job`: the user database, the process table, audit sessions and
//! Seatbelt. All of the mode's `unsafe` code is here.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::mem;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::ptr;

use crate::mac_job::sweep::Proc;

/// A user, from the user database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct User {
    /// Its id.
    pub uid: u32,
    /// Its primary group.
    pub gid: u32,
    /// Its home folder.
    pub home: String,
}

/// `au_mask_t` of `<bsm/audit.h>`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct AuMask {
    am_success: u32,
    am_failure: u32,
}

/// `au_tid_addr_t` of `<bsm/audit.h>` (`at_port` is a `dev_t`, 32 bits on macOS).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct AuTidAddr {
    at_port: i32,
    at_type: u32,
    at_addr: [u32; 4],
}

/// `auditinfo_addr_t` of `<bsm/audit.h>`.
#[repr(C)]
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

/// The group of macOS's administrators.
pub(super) const ADMIN_GID: u32 = 80;

// FFI-BOUNDARY: the audit calls read or write the one struct they are given, owned by the
// caller, of the length given; they keep no pointer. `sandbox_init_with_parameters` reads the
// profile text and the NULL-ended array of parameter strings, all owned by the caller and kept
// by nobody after it returns; on a failure it stores in `errorbuf` a string the library
// allocated, which the caller frees with `sandbox_free_error` and nothing else.
// `sandbox_init_with_parameters` is exported by libsystem_sandbox (part of libSystem) but is not
// in the public `<sandbox.h>`; its signature is the one Seatbelt's own tools use.
unsafe extern "C" {
    fn getaudit_addr(info: *mut AuditinfoAddr, length: c_int) -> c_int;
    fn setaudit_addr(info: *const AuditinfoAddr, length: c_int) -> c_int;
    fn auditon(cmd: c_int, data: *mut c_void, length: c_int) -> c_int;
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// The size of `T` as the C calls take it.
fn c_len<T>() -> c_int {
    // The structs here are tens of bytes.
    c_int::try_from(mem::size_of::<T>()).unwrap_or(c_int::MAX)
}

/// The user named `name`.
pub(super) fn user(name: &str) -> io::Result<User> {
    let c_name = CString::new(name).map_err(|_| io::Error::other("a user name with a NUL"))?;
    let mut buf = vec![0 as c_char; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value of the plain C struct; it is only written by
    // `getpwnam_r` below.
    let mut pwd: libc::passwd = unsafe { mem::zeroed() };
    let mut result: *mut libc::passwd = ptr::null_mut();
    // SAFETY: every pointer is to memory of ours that outlives the call, `buf` of the length
    // given; `getpwnam_r` writes the record into `pwd`, its strings into `buf`, and sets `result`
    // to `&pwd` or to null.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    if result.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no user {name}"),
        ));
    }
    // SAFETY: `getpwnam_r` succeeded, so `pw_dir` points at a NUL-ended string in `buf`, which
    // is alive here.
    let home = unsafe { CStr::from_ptr(pwd.pw_dir) }
        .to_string_lossy()
        .into_owned();
    Ok(User {
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home,
    })
}

/// The groups of user `name` whose primary group is `gid`.
pub(super) fn groups(name: &str, gid: u32) -> io::Result<Vec<u32>> {
    let c_name = CString::new(name).map_err(|_| io::Error::other("a user name with a NUL"))?;
    let base = c_int::try_from(gid).map_err(|_| io::Error::other("a group id over 2^31"))?;
    let mut list = vec![0 as c_int; 1024];
    let mut count = c_int::try_from(list.len()).unwrap_or(c_int::MAX);
    // SAFETY: `list` holds `count` entries and outlives the call, which writes at most that many
    // and sets `count` to how many it wrote.
    let rc = unsafe { libc::getgrouplist(c_name.as_ptr(), base, list.as_mut_ptr(), &mut count) };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "the groups of {name} do not fit 1024 entries"
        )));
    }
    let count = usize::try_from(count).unwrap_or(0).min(list.len());
    Ok(list[..count]
        .iter()
        .filter_map(|&g| u32::try_from(g).ok())
        .collect())
}

/// The number of threads of this process.
pub(super) fn own_threads() -> io::Result<i32> {
    // SAFETY: an all-zero `proc_taskinfo` is a valid value of the plain C struct.
    let mut info: libc::proc_taskinfo = unsafe { mem::zeroed() };
    let len = c_len::<libc::proc_taskinfo>();
    // SAFETY: `info` is ours and `len` bytes long; `proc_pidinfo` writes at most that many.
    let got = unsafe {
        libc::proc_pidinfo(
            std::process::id().cast_signed(),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            len,
        )
    };
    if got != len {
        return Err(io::Error::last_os_error());
    }
    Ok(info.pti_threadnum)
}

/// Every live process (zombies left out), with its audit session when `asids` is set.
pub(super) fn processes(asids: bool) -> io::Result<Vec<Proc>> {
    // SAFETY: a null buffer of size 0 asks for the number of processes only.
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0 as c_int; usize::try_from(count).unwrap_or(0) + 256];
    let bytes = c_int::try_from(pids.len() * mem::size_of::<c_int>()).unwrap_or(c_int::MAX);
    // SAFETY: `pids` is `bytes` long and ours; the call writes at most that many bytes and returns
    // how many pids it wrote.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(usize::try_from(got).unwrap_or(0));
    let mut list = Vec::with_capacity(pids.len());
    for pid in pids {
        let Some(uid) = live_uid(pid) else { continue };
        let asid = if asids { asid_of(pid) } else { None };
        list.push(Proc { pid, uid, asid });
    }
    Ok(list)
}

/// The effective user of process `pid`, or `None` when it is gone or a zombie.
fn live_uid(pid: c_int) -> Option<u32> {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid value of the plain C struct.
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let len = c_len::<libc::proc_bsdinfo>();
    // SAFETY: `info` is ours and `len` bytes long; `proc_pidinfo` writes at most that many.
    let got =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), len) };
    (got == len && info.pbi_status != libc::SZOMB).then_some(info.pbi_uid)
}

/// The audit session of process `pid`, or `None` when it cannot be read.
pub(super) fn asid_of(pid: c_int) -> Option<i32> {
    let mut info = AuditpinfoAddr {
        ap_pid: pid,
        ..AuditpinfoAddr::default()
    };
    // SAFETY: `info` is ours and of the length given; `auditon` reads `ap_pid` and writes the
    // rest of the struct.
    let rc = unsafe {
        auditon(
            A_GETPINFO_ADDR,
            (&raw mut info).cast(),
            c_len::<AuditpinfoAddr>(),
        )
    };
    (rc == 0).then_some(info.ap_asid)
}

/// Clears every file flag of `path` (a link itself, not what it points at): a user-immutable or
/// append-only flag a job set would otherwise stop the folder's removal.
pub(super) fn clear_flags(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a path with a NUL"))?;
    // SAFETY: `c_path` is a NUL-ended string of ours, alive for the call, which only reads it.
    if unsafe { libc::lchflags(c_path.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends SIGKILL to `pid`. A process already gone is not an error.
pub(super) fn kill(pid: c_int) {
    // SAFETY: `kill` takes plain values and touches no memory of ours.
    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// A pipe whose two ends close when this process executes a program: (read, write).
pub(super) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` is ours and holds the two entries `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` succeeded, so both are open descriptors that nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in fds {
        // SAFETY: `fd` is open (owned above); `fcntl` with these commands takes plain values.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

/// What the child does between `fork` and the program: everything is made before the fork.
pub(super) struct ChildSteps {
    /// Open a new audit session first.
    pub new_session: bool,
    /// Where the new session's id is written, as 4 bytes.
    pub asid_out: RawFd,
    /// The user and its primary group, its only group.
    pub uid: u32,
    /// The primary group.
    pub gid: u32,
    /// The profile text.
    pub profile: CString,
    /// The profile's parameter name.
    pub param_name: CString,
    /// Its value.
    pub param_value: CString,
}

impl ChildSteps {
    /// Runs the steps in the child: opens the audit session, drops to the user for good, and
    /// applies the profile. A profile the library refuses writes its error to standard error.
    ///
    /// # Safety
    /// Only in the child of a `fork` of a process that had one thread (the caller checks
    /// [`own_threads`]): `sandbox_init_with_parameters` allocates, which is safe after `fork`
    /// only when no other thread could have held the allocator's lock.
    pub(super) unsafe fn run(&self) -> io::Result<()> {
        if self.new_session {
            let mut info = AuditinfoAddr::default();
            // SAFETY: `info` is ours and of the length given.
            if unsafe { getaudit_addr(&mut info, c_len::<AuditinfoAddr>()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            info.ai_asid = AU_ASSIGN_ASID;
            info.ai_auid = self.uid;
            // SAFETY: as above; the call only reads `info`.
            if unsafe { setaudit_addr(&info, c_len::<AuditinfoAddr>()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above.
            if unsafe { getaudit_addr(&mut info, c_len::<AuditinfoAddr>()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let bytes = info.ai_asid.to_ne_bytes();
            // SAFETY: `bytes` is ours and 4 bytes long; `asid_out` is the write end of a pipe the
            // parent keeps open until the start returns.
            let _ = unsafe { libc::write(self.asid_out, bytes.as_ptr().cast(), bytes.len()) };
        }
        let groups = [self.gid];
        // SAFETY: `groups` is ours and holds the 1 entry given; the other calls take plain values.
        unsafe {
            if libc::setgroups(1, groups.as_ptr()) != 0
                || libc::setgid(self.gid) != 0
                || libc::setuid(self.uid) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::setuid(0) == 0 {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
        }
        let params = [
            self.param_name.as_ptr(),
            self.param_value.as_ptr(),
            ptr::null(),
        ];
        let mut error: *mut c_char = ptr::null_mut();
        // SAFETY: the text and the NULL-ended array point at strings of `self`, alive for the
        // call; `error` is ours (see the FFI-BOUNDARY comment).
        let rc = unsafe {
            sandbox_init_with_parameters(self.profile.as_ptr(), 0, params.as_ptr(), &mut error)
        };
        if rc != 0 {
            if !error.is_null() {
                const LEAD: &[u8] = b"js-agent: the Seatbelt profile was refused: ";
                // SAFETY: `error` is the NUL-ended string the library made; it is written to
                // standard error, then freed once, the way the library says.
                unsafe {
                    let text = CStr::from_ptr(error).to_bytes();
                    let _ = libc::write(2, LEAD.as_ptr().cast(), LEAD.len());
                    let _ = libc::write(2, text.as_ptr().cast(), text.len());
                    let _ = libc::write(2, b"\n".as_ptr().cast(), 1);
                    sandbox_free_error(error);
                }
            }
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(())
    }
}
