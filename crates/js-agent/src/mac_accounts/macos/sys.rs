//! The macOS calls that are neither the directory service nor the process table: the
//! lookups by name and uid, `launchctl` and the other programs, the privacy check, and two
//! calls on files.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::sync::OnceLock;
use std::time::Instant;

use js_mac_session::disclaim::Program;
use rustix::io::Errno;

use super::super::host::{CommandOutput, PrintAnswer, UserRecord};

/// The directory of the privacy database, which only a process with Full Disk Access may
/// list.
const PRIVACY_DIR: &str = "/Library/Application Support/com.apple.TCC";

/// Lists [`PRIVACY_DIR`] (opens the directory and reads one entry name: no file in it is
/// opened and no database is read).
pub(super) fn full_disk_access() -> io::Result<()> {
    std::fs::read_dir(PRIVACY_DIR)?
        .next()
        .transpose()
        .map(|_| ())
}

/// The record named `name`.
pub(super) fn account(name: &str) -> io::Result<Option<UserRecord>> {
    let c_name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name holds NUL"))?;
    // SAFETY: a zeroed passwd is a valid value for getpwnam_r to overwrite.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 1 << 16];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live, writable value of the right type, the name is
    // NUL-terminated, and `buffer.len()` is the buffer's size.
    let code = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    Ok((!found.is_null()).then_some(UserRecord {
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    }))
}

/// Whether a record has the uid `uid`.
pub(super) fn uid_taken(uid: u32) -> io::Result<bool> {
    // SAFETY: a zeroed passwd is a valid value for getpwuid_r to overwrite.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 1 << 16];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live, writable value of the right type, and
    // `buffer.len()` is the buffer's size.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    Ok(!found.is_null())
}

/// `launchctl print <domain>`, with no environment of ours and no input.
pub(super) fn launchctl_print(domain: &str) -> io::Result<PrintAnswer> {
    let output = Program::new("/bin/launchctl", ["print", domain])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(PrintAnswer {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `launchctl bootout <domain>`.
pub(super) fn bootout(domain: &str) -> io::Result<()> {
    let output = Program::new("/bin/launchctl", ["bootout", domain])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Runs `program` with `args`, with no environment of ours and no input, and collects what
/// it said.
pub(super) fn run(program: &str, args: &[&str]) -> io::Result<CommandOutput> {
    let output = Program::new(program, args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Milliseconds since the first call, on a clock that only goes forward.
pub(super) fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The user flags that block removing an entry or a directory's entries.
const LOCKING_FLAGS: u32 = libc::UF_IMMUTABLE | libc::UF_APPEND;

/// Clears [`LOCKING_FLAGS`] of `name` in `dir`, whose flags are `flags`, without following a
/// link (`setattrlistat` with `FSOPT_NOFOLLOW`; macOS has no `chflagsat`). Nothing is called
/// when none of them is set.
pub(crate) fn clear_user_flags(dir: &OwnedFd, name: &OsStr, flags: u32) -> io::Result<()> {
    if flags & LOCKING_FLAGS == 0 {
        return Ok(());
    }
    let name = CString::new(name.as_bytes())?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_FLAGS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut value: u32 = flags & !LOCKING_FLAGS;
    // SAFETY: `name` is NUL-terminated, `list` an attrlist that asks for the common flags
    // only, and `value` the u32 that attribute is; all three outlive the call.
    let set = unsafe {
        libc::setattrlistat(
            dir.as_raw_fd(),
            name.as_ptr(),
            (&raw mut list).cast(),
            (&raw mut value).cast(),
            std::mem::size_of::<u32>(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `renameatx_np` with `RENAME_EXCL`: does not replace what is at the new name.
pub(crate) fn rename_excl(
    from: &OwnedFd,
    old: &OsStr,
    to: &OwnedFd,
    new: &OsStr,
) -> Result<(), Errno> {
    let (Ok(old), Ok(new)) = (CString::new(old.as_bytes()), CString::new(new.as_bytes())) else {
        return Err(Errno::INVAL);
    };
    // SAFETY: both names are NUL-terminated and outlive the call, both descriptors are open
    // directories borrowed for it.
    let renamed = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            old.as_ptr(),
            to.as_raw_fd(),
            new.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if renamed == 0 {
        return Ok(());
    }
    Err(Errno::from_raw_os_error(
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    ))
}
