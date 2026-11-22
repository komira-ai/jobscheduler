//! The macOS implementation of [`AccountHost`]: user records through OpenDirectory's local
//! node, launchd through `launchctl`, processes through `libproc` and `kill`, and the few
//! calls on files that only macOS has (the user flags and the ACLs that block a removal, a
//! rename that does not replace), and the start of a program as an account for
//! `js-agent __account-cycle`.
//!
//! Nothing here is reached by the Linux test jobs; the pure decisions it serves are tested
//! there over a fake host.

mod as_account;
mod directory;
mod procs;
mod sys;

use std::io;
use std::path::Path;

pub(super) use as_account::AsAccount;
pub(super) use sys::{clear_user_flags, rename_excl, strip_acl};

use super::host::{
    AccountHost, CommandOutput, LiveProcess, NewUser, PrintAnswer, UserRecord, merge_record,
    present_in_any,
};

/// The macOS host.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacHost;

impl AccountHost for MacHost {
    fn full_disk_access(&self) -> io::Result<()> {
        sys::full_disk_access()
    }

    fn account(&self, name: &str) -> io::Result<Option<UserRecord>> {
        merge_record(directory::exists(name), sys::account(name))
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        present_in_any(directory::uid_exists(uid), sys::uid_taken(uid))
    }

    fn guid_taken(&self, guid: &str) -> io::Result<bool> {
        directory::guid_exists(guid)
    }

    fn record_guid(&self, name: &str) -> io::Result<Option<String>> {
        directory::guid_of(name)
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

    /// Deletes the record, then flushes the user lookup cache: `getpwnam` answers from a cache
    /// that keeps a deleted record for a while (measured on macOS 27: the proof's lookup by name
    /// still found an account the directory no longer held), and the proof reads both sources.
    fn delete_account(&self, name: &str) -> io::Result<bool> {
        let deleted = directory::delete(name)?;
        let flushed = sys::run("/usr/bin/dscacheutil", &["-flushcache"])?;
        if !flushed.success {
            return Err(io::Error::other(format!(
                "dscacheutil -flushcache: {}",
                flushed.stderr.trim()
            )));
        }
        Ok(deleted)
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
