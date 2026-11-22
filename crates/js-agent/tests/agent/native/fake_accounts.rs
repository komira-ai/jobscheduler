//! A fake of the operating system under the account pool ([`AccountHost`]): user records in
//! memory, homes as directories of the test, and no process, launchd domain or IPC object.
//! The pool's own file work (the homes' removal, the per-user folders, the shared places) runs
//! for real on the test's directories, so a retirement is shown gone as on a Mac.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent::mac_accounts::{
    AccountHost, CommandOutput, LiveProcess, NewUser, PrintAnswer, UserRecord,
};

/// What the fake keeps.
#[derive(Default)]
struct State {
    /// The records, by name.
    users: BTreeMap<String, NewUser>,
    /// Names whose record cannot be deleted: a retirement holds their account.
    undeletable: BTreeSet<String>,
    /// No record can be made: a lend with none ready fails.
    no_records: bool,
    /// A home made from now on holds a file named `exec`, so the run directory cannot be
    /// made.
    exec_in_home: bool,
    /// Whether Full Disk Access is held (`None`) or refused how.
    refused: Option<io::ErrorKind>,
}

/// The fake host. Shared by the pool and the test.
#[derive(Default)]
pub struct FakeAccounts {
    state: Mutex<State>,
    clock: AtomicU64,
}

impl FakeAccounts {
    /// A fake with no record, that holds Full Disk Access.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The names of the records there are now.
    pub fn records(&self) -> Vec<String> {
        self.state().users.keys().cloned().collect()
    }

    /// The record named `name` can no longer be deleted.
    pub fn keep_record(&self, name: &str) {
        self.state().undeletable.insert(name.to_owned());
    }

    /// The record named `name` can be deleted again.
    pub fn free_record(&self, name: &str) {
        self.state().undeletable.remove(name);
    }

    /// No record can be made from now on.
    pub fn refuse_records(&self) {
        self.state().no_records = true;
    }

    /// Every home made from now on holds a file named `exec`.
    pub fn plant_exec(&self) {
        self.state().exec_in_home = true;
    }

    /// Full Disk Access is refused with `kind` from now on (`PermissionDenied` is "not
    /// granted").
    pub fn refuse_access(&self, kind: io::ErrorKind) {
        self.state().refused = Some(kind);
    }
}

impl AccountHost for FakeAccounts {
    fn full_disk_access(&self) -> io::Result<()> {
        match self.state().refused {
            None => Ok(()),
            Some(kind) => Err(io::Error::new(
                kind,
                "the privacy database cannot be listed",
            )),
        }
    }

    fn account(&self, name: &str) -> io::Result<Option<UserRecord>> {
        Ok(self.state().users.get(name).map(|user| UserRecord {
            uid: user.uid,
            gid: user.gid,
        }))
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        Ok(self.state().users.values().any(|user| user.uid == uid))
    }

    fn in_directory(&self, name: &str) -> io::Result<bool> {
        Ok(self.state().users.contains_key(name))
    }

    fn uid_in_directory(&self, uid: u32) -> io::Result<bool> {
        self.uid_taken(uid)
    }

    fn guid_taken(&self, guid: &str) -> io::Result<bool> {
        Ok(self
            .state()
            .users
            .values()
            .any(|user| user.generated_uid == guid))
    }

    fn record_guid(&self, name: &str) -> io::Result<Option<String>> {
        Ok(self
            .state()
            .users
            .get(name)
            .map(|user| user.generated_uid.clone()))
    }

    fn list_accounts(&self, prefix: &str) -> io::Result<Vec<(String, u32)>> {
        Ok(self
            .state()
            .users
            .values()
            .filter(|user| user.name.starts_with(prefix))
            .map(|user| (user.name.clone(), user.uid))
            .collect())
    }

    fn make_home(&self, homes: &Path, name: &str, _uid: u32, _gid: u32) -> io::Result<()> {
        let home = homes.join(name);
        fs::create_dir(&home)?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        if self.state().exec_in_home {
            fs::write(home.join("exec"), b"")?;
        }
        Ok(())
    }

    fn create_account(&self, user: &NewUser) -> io::Result<()> {
        let mut state = self.state();
        if state.no_records {
            return Err(io::Error::other("the directory refused the record"));
        }
        state.users.insert(user.name.clone(), user.clone());
        Ok(())
    }

    fn delete_account(&self, name: &str) -> io::Result<bool> {
        let mut state = self.state();
        if state.undeletable.contains(name) {
            return Err(io::Error::other("the directory refused the deletion"));
        }
        Ok(state.users.remove(name).is_some())
    }

    /// The fake lists no process: the processes the tests start run as the test's own uid,
    /// which the pool must never be told to kill.
    fn processes(&self, _uid: u32) -> io::Result<Vec<LiveProcess>> {
        Ok(Vec::new())
    }

    fn kill_all(&self, _uid: u32) -> io::Result<()> {
        Ok(())
    }

    /// No domain prints: launchd's answer for a domain that is not there.
    fn launchctl_print(&self, _domain: &str) -> io::Result<PrintAnswer> {
        Ok(PrintAnswer {
            code: Some(113),
            stdout: String::new(),
            stderr: "Could not find service".to_owned(),
        })
    }

    fn bootout(&self, _domain: &str) -> io::Result<()> {
        Err(io::Error::other("Boot-out failed: 3: No such process"))
    }

    /// `ipcs -a` lists nothing; any other program succeeds and says nothing.
    fn command(&self, _program: &str, _args: &[&str]) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn pause(&self) {}

    fn now_ms(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }
}
