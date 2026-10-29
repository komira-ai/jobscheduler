//! The system calls the probes need that `libc` does not declare: `clonefile`,
//! `fclonefileat`, `proc_pid_rusage` and `mach_timebase_info`, all in libSystem.

use std::ffi::{CString, c_char, c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

unsafe extern "C" {
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
    fn fclonefileat(srcfd: c_int, dst_dirfd: c_int, dst: *const c_char, flags: u32) -> c_int;
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
    fn mach_timebase_info(info: *mut [u32; 2]) -> c_int;
}

/// `RUSAGE_INFO_V2`: user and system time, page-ins, wired, resident and footprint sizes.
const RUSAGE_INFO_V2: c_int = 2;
/// `_CS_DARWIN_USER_TEMP_DIR`: the uid's `/var/folders/<xx>/<hash>/T/`.
pub const CS_USER_TEMP_DIR: c_int = 65537;

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}

fn status(rc: c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `clonefile(src, dst, 0)`.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (cpath(src)?, cpath(dst)?);
    // SAFETY: both are NUL-terminated and outlive the call.
    status(unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) })
}

/// `fclonefileat(src, AT_FDCWD, dst, 0)`; `dst` is absolute.
pub fn clone_fd(src: &File, dst: &Path) -> io::Result<()> {
    let dst = cpath(dst)?;
    // SAFETY: the descriptor is open for the call and `dst` is NUL-terminated.
    status(unsafe { fclonefileat(src.as_raw_fd(), libc::AT_FDCWD, dst.as_ptr(), 0) })
}

/// The part of `rusage_info_v2` the runtime would read, in mach absolute-time ticks and bytes.
#[derive(Debug, Clone, Copy)]
pub struct PidUsage {
    /// CPU time in user mode.
    pub user_time: u64,
    /// CPU time in the kernel.
    pub system_time: u64,
    /// Resident set.
    pub resident: u64,
    /// Physical footprint.
    pub footprint: u64,
}

/// `proc_pid_rusage(pid, RUSAGE_INFO_V2)`.
pub fn pid_usage(pid: i32) -> io::Result<PidUsage> {
    let mut buf = [0u64; 64];
    // SAFETY: the buffer is 512 bytes; `rusage_info_v2` is 144.
    status(unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, buf.as_mut_ptr().cast()) })?;
    // Layout: ri_uuid (2 words), user_time, system_time, pkg_idle_wkups, interrupt_wkups,
    // pageins, wired_size, resident_size, phys_footprint.
    Ok(PidUsage {
        user_time: buf[2],
        system_time: buf[3],
        resident: buf[8],
        footprint: buf[9],
    })
}

/// The mach absolute-time ratio (numerator, denominator): ticks times numer over denom is ns.
pub fn timebase() -> io::Result<(u32, u32)> {
    let mut info = [0u32; 2];
    // SAFETY: `info` is the two-word `mach_timebase_info_data_t`.
    status(unsafe { mach_timebase_info(&raw mut info) })?;
    Ok((info[0], info[1]))
}

/// `confstr(name)` as a path.
pub fn confstr_path(name: c_int) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: the buffer and its length are as passed.
    let n = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return Err(io::Error::other(format!("confstr({name}) returned {n}")));
    }
    buf.truncate(n - 1);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// The file system type name (`apfs`, `hfs`, ...) holding `path`.
pub fn fs_type(path: &Path) -> io::Result<String> {
    let path = cpath(path)?;
    // SAFETY: `statfs` is plain data; zero is a valid starting value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `st` is a `statfs`.
    status(unsafe { libc::statfs(path.as_ptr(), &raw mut st) })?;
    // SAFETY: the kernel NUL-terminates `f_fstypename`.
    let name = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) };
    Ok(name.to_string_lossy().into_owned())
}
