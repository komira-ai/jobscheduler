//! The system calls: `statfs` and `sysctl`. Nothing here owns memory past a call: the
//! buffers are on the stack and the path is a `CString` that lives through the call.

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use super::securelevel::{SecurelevelError, check_securelevel};
use super::volume::{VolumeError, VolumeFacts, check_volumes};

/// `statfs` of `path`, as the facts the volume checks read.
///
/// # Errors
/// The path cannot be read.
pub fn statfs_facts(path: &Path) -> io::Result<VolumeFacts> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: `statfs` is plain data, for which all zero bytes is a valid value.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and outlives the call; `buf` is a `statfs` the
    // kernel fills.
    if unsafe { libc::statfs(path.as_ptr(), &raw mut buf) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel NUL-terminates the type name inside its 16 bytes.
    let fs_type = unsafe { CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    Ok(VolumeFacts {
        fs_type: fs_type.to_string_lossy().into_owned(),
        flags: buf.f_flags,
        fsid: buf.f_fsid.val,
    })
}

/// `kern.securelevel`.
///
/// # Errors
/// The kernel did not answer.
pub fn securelevel() -> io::Result<i64> {
    let mut value: libc::c_int = 0;
    let mut len: libc::size_t = size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated; `value` is a `c_int` and `len` its size; nothing
    // is set (null new value of length 0).
    let done = unsafe {
        libc::sysctlbyname(
            c"kern.securelevel".as_ptr(),
            (&raw mut value).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if done == 0 {
        Ok(i64::from(value))
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The start checks failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The store volume is not fit.
    #[error(transparent)]
    Volume(#[from] VolumeError),
    /// The kernel's securelevel is too high.
    #[error(transparent)]
    Securelevel(#[from] SecurelevelError),
}

/// Runs the checks the store makes before it starts, against the real system: the volumes
/// of `store` and `scratch`, then the securelevel.
///
/// # Errors
/// The first check that fails.
pub fn start_checks(store: &Path, scratch: &Path) -> Result<(), StartError> {
    check_volumes(store, scratch, statfs_facts)?;
    check_securelevel(securelevel)?;
    Ok(())
}
