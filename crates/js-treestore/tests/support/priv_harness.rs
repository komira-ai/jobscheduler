//! The two pieces of the privileged tests' harness that a wrong value breaks silently on a
//! provisioned Mac: the command that runs something as the unprivileged user, and the text
//! of the ACL entry given to that user. They are compiled into `tests/privileged.rs` and
//! into the ordinary test binary, which checks them on every platform.
#![allow(dead_code)]

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// The command that runs `program` as `uid`:`gid`. The working directory is `/`: the child
/// inherits the root test process's directory, which the unprivileged user may not be able
/// to read, and some programs refuse to start without a readable one.
///
/// The user and group are set only where they differ from the test process's own. Setting
/// them makes the standard library fork the whole test process and change them in the copy
/// before the exec, where it otherwise spawns the program directly (`posix_spawn`). A fork
/// of a test binary whose other tests run on other threads is unsafe: on macOS the copy can
/// abort inside `fork` on a one-time initialisation another thread was part way through,
/// and until its exec it holds every file the process has open, store locks included. As
/// the process's own user the two calls would change nothing, so they are left out; the
/// privileged tests, which change user, run on one thread.
pub fn user_command(program: &str, args: &[&Path], uid: u32, gid: u32) -> Command {
    let mut command = Command::new(program);
    command.args(args).current_dir("/").stdin(Stdio::null());
    if uid != rustix::process::geteuid().as_raw() {
        command.uid(uid);
    }
    if gid != rustix::process::getegid().as_raw() {
        command.gid(gid);
    }
    command
}

/// The name of user `uid`, if there is one.
pub fn user_name(uid: u32) -> Option<String> {
    // SAFETY: `passwd` is plain data (integers and pointers), for which all zero bytes is
    // a valid value.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: `entry`, `buf` and `found` outlive the call; the length passed is the
    // buffer's; the strings `entry` points to live in `buf`, which is read below before it
    // is dropped.
    let done = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut entry,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut found,
        )
    };
    if done != 0 || found.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` is a NUL-terminated string inside `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

/// The entry for `chmod +a` that lets user `uid` write a directory and what is made in it
/// inherits. The entry names the user: `chmod +a` does not translate `#<uid>`.
///
/// # Panics
/// There is no user with that uid.
pub fn inheritable_write_ace(uid: u32) -> String {
    let name = user_name(uid).unwrap_or_else(|| panic!("no user with uid {uid}"));
    format!("user:{name} allow write,file_inherit,directory_inherit")
}
