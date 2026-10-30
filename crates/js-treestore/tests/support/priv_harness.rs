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
pub fn user_command(program: &str, args: &[&Path], uid: u32, gid: u32) -> Command {
    let mut command = Command::new(program);
    command.args(args).uid(uid).gid(gid).stdin(Stdio::null());
    command
}

/// The name of user `uid`, if there is one.
pub fn user_name(_uid: u32) -> Option<String> {
    None
}

/// The entry for `chmod +a` that lets user `uid` write a directory and what is made in it
/// inherits. The entry names the user: `chmod +a` does not translate `#<uid>`.
pub fn inheritable_write_ace(uid: u32) -> String {
    format!("#{uid} allow write,file_inherit,directory_inherit")
}
