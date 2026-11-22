//! The macOS calls that are neither the directory service nor the process table: the
//! lookups by name and uid, `launchctl` and the other programs, the privacy check, and three
//! calls on files.

use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_void};
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
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

/// `ACL_TYPE_EXTENDED` of `<sys/acl.h>`: the access control list macOS keeps on a file.
const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;

// FFI-BOUNDARY: `acl_init` returns an ACL the caller owns and frees once with `acl_free`;
// `acl_set_link_np` reads the NUL-terminated path and the ACL, both owned by the caller, and
// keeps neither. These are in libSystem.
unsafe extern "C" {
    fn acl_init(count: c_int) -> *mut c_void;
    fn acl_free(object: *mut c_void) -> c_int;
    fn acl_set_link_np(path: *const c_char, kind: c_int, acl: *mut c_void) -> c_int;
}

/// The path the open directory `dir` is at (`F_GETPATH`).
fn path_of(dir: &OwnedFd) -> io::Result<PathBuf> {
    let mut buffer = [0 as c_char; libc::PATH_MAX as usize];
    // SAFETY: `F_GETPATH` writes at most `PATH_MAX` bytes, NUL-terminated, into the buffer it
    // is given, which is that long; the descriptor is open for the call.
    let found = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if found == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above wrote a NUL-terminated string into `buffer`, which is alive.
    let bytes = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
    Ok(Path::new(OsStr::from_bytes(bytes)).to_path_buf())
}

/// Replaces the ACL of `path` with an empty one, which removes it, without following a link
/// at its end. An entry that is not there is done.
fn clear_acl(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `acl_init` takes a count; a null answer is an error, checked below.
    let acl = unsafe { acl_init(0) };
    if acl.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `path` is NUL-terminated and `acl` is the one `acl_init` made; both outlive the
    // call, which keeps neither.
    let set = unsafe { acl_set_link_np(path.as_ptr(), ACL_TYPE_EXTENDED, acl) };
    let failure = io::Error::last_os_error();
    // SAFETY: `acl` was made by `acl_init` above and is freed here once.
    unsafe { acl_free(acl) };
    match set {
        0 => Ok(()),
        _ if failure.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(failure),
    }
}

/// Strips the ACL of the entry `name` of `dir` and of `dir` itself: an ACL that denies
/// `delete` on the entry or `delete_child` on its directory refuses the removal, root's too,
/// until it is gone. Neither is followed if it is a link (`acl_set_link_np`). Both are
/// tried, and either one stripped is enough to try the removal again: the directory is often
/// a top-level directory of a per-user folder (`0`, `C`, `T`, `X`), whose own ACL System
/// Integrity Protection refuses to change (measured: "Failed to clear ACL ... Operation not
/// permitted"), while the entry's is the job's and can be stripped.
///
/// # Errors
/// Neither ACL could be stripped; the entry's failure is named first.
pub(crate) fn strip_acl(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let folder = path_of(dir)?;
    let entry = clear_acl(&folder.join(name));
    let directory = clear_acl(&folder);
    match (entry, directory) {
        (Err(entry), Err(directory)) => Err(io::Error::new(
            entry.kind(),
            format!("{entry}; its directory: {directory}"),
        )),
        _ => Ok(()),
    }
}
