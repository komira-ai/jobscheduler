//! The macOS implementation of [`AccountHost`]: user records through OpenDirectory's local
//! node, launchd through `launchctl`, processes through `libproc` and `kill`, and the few
//! calls on files that only macOS has (the user flags that block a removal, a rename that
//! does not replace).
//!
//! Nothing here is reached by the Linux test jobs; the pure decisions it serves are tested
//! there over a fake host.

mod directory;
mod procs;
mod sys;

use std::io;
use std::path::Path;

pub(super) use sys::{clear_user_flags, rename_excl};

use super::host::{AccountHost, CommandOutput, LiveProcess, NewUser, PrintAnswer, UserRecord};

/// The macOS host.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacHost;

impl AccountHost for MacHost {
    fn full_disk_access(&self) -> io::Result<()> {
        sys::full_disk_access()
    }

    fn account(&self, name: &str) -> io::Result<Option<UserRecord>> {
        sys::account(name)
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        sys::uid_taken(uid)
    }

    fn list_accounts(&self, prefix: &str) -> io::Result<Vec<(String, u32)>> {
        directory::list(prefix)
    }

    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
        super::make_home(homes, name, uid, gid)
    }

    fn create_account(&self, user: &NewUser) -> io::Result<()> {
        directory::create(user)
    }

    fn delete_account(&self, name: &str) -> io::Result<bool> {
        directory::delete(name)
    }

    fn processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>> {
        procs::live_processes(uid)
    }

    fn kill_all(&self, uid: u32) -> io::Result<()> {
        procs::kill_all(uid)
    }

    fn launchctl_print(&self, domain: &str) -> io::Result<PrintAnswer> {
        sys::launchctl_print(domain)
    }

    fn bootout(&self, domain: &str) -> io::Result<()> {
        sys::bootout(domain)
    }

    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        sys::run(program, args)
    }

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    fn now_ms(&self) -> u64 {
        sys::now_ms()
    }
}
