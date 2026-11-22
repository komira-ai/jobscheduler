//! An [`Os`] that models what macOS does to a removal and a test cannot make: entries whose
//! removal is refused until their user flags are cleared (`uchg`, `uappnd`) or their ACL is
//! stripped, flags or ACLs that cannot be lifted, sealed entries (a data vault) and pinned ones (`sunlnk`). Everything else is the real call.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustix::fs::{AtFlags, Stat};
use rustix::io::Errno;

use super::super::tree::Os;

/// The entries the fake treats as locked, by name (a test gives each entry its own name), and
/// every call made.
#[derive(Default)]
struct State {
    /// Refuse removal (`EPERM`) until their flags are cleared.
    flagged: BTreeSet<OsString>,
    /// Refuse removal (`EACCES`) until their ACL is stripped.
    denied: BTreeSet<OsString>,
    /// Their flags cannot be cleared.
    unclearable: BTreeSet<OsString>,
    /// Their ACL cannot be stripped.
    unstrippable: BTreeSet<OsString>,
    /// Sealed: root is refused them (`EPERM`), and their removal too.
    sealed: BTreeSet<OsString>,
    /// Pinned: their own removal is refused (`EPERM`); what is in them can go.
    pinned: BTreeSet<OsString>,
    /// The calls, `<call> <name>`, in order.
    calls: Vec<String>,
}

/// The fake. A call that finds nothing locked is the real one. Clones share what is locked
/// and the calls made, so a test keeps one and gives the pool another.
#[derive(Clone, Default)]
pub(in super::super) struct FakeOs {
    state: Arc<Mutex<State>>,
}

impl FakeOs {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fake that locks nothing.
    pub(in super::super) fn new() -> Self {
        Self::default()
    }

    /// The entry `name` has a user flag: its removal is refused until the flags are cleared.
    pub(in super::super) fn flag(&self, name: &str) {
        self.state().flagged.insert(name.into());
    }

    /// The entry `name` has an ACL that denies its removal: refused until it is stripped.
    pub(in super::super) fn deny(&self, name: &str) {
        self.state().denied.insert(name.into());
    }

    /// The flags of `name` cannot be cleared (the call fails); it stays flagged.
    pub(in super::super) fn flag_for_good(&self, name: &str) {
        self.flag(name);
        self.state().unclearable.insert(name.into());
    }

    /// The ACL of `name` cannot be stripped (the call fails); it stays denied.
    pub(in super::super) fn deny_for_good(&self, name: &str) {
        self.deny(name);
        self.state().unstrippable.insert(name.into());
    }

    /// The entry `name` is sealed, as a data vault is: root is refused it.
    pub(in super::super) fn seal(&self, name: &str) {
        self.state().sealed.insert(name.into());
    }

    /// The entry `name` carries the system's no-unlink flag: its removal is refused.
    pub(in super::super) fn pin(&self, name: &str) {
        self.state().pinned.insert(name.into());
    }

    /// The calls made, `<call> <name>`, in order.
    pub(in super::super) fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    fn log(&self, call: &str, name: &OsStr) {
        self.state()
            .calls
            .push(format!("{call} {}", name.to_string_lossy()));
    }
}

impl Os for FakeOs {
    fn unlink(&self, dir: &OwnedFd, name: &OsStr, flags: AtFlags) -> Result<(), Errno> {
        self.log("unlink", name);
        let state = self.state();
        if state.flagged.contains(name) | state.sealed.contains(name) | state.pinned.contains(name)
        {
            return Err(Errno::PERM);
        }
        if state.denied.contains(name) {
            return Err(Errno::ACCESS);
        }
        drop(state);
        rustix::fs::unlinkat(dir, name, flags)
    }

    fn clear_flags(&self, _dir: &OwnedFd, name: &OsStr, _stat: &Stat) -> io::Result<()> {
        self.log("clear_flags", name);
        let mut state = self.state();
        if state.unclearable.contains(name) {
            return Err(io::Error::other("the flags cannot be cleared"));
        }
        state.flagged.remove(name);
        Ok(())
    }

    fn strip_acl(&self, _dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
        self.log("strip_acl", name);
        let mut state = self.state();
        if state.unstrippable.contains(name) {
            return Err(io::Error::other("the ACL cannot be stripped"));
        }
        state.denied.remove(name);
        Ok(())
    }

    fn sealed(&self, _dir: &OwnedFd, name: &OsStr) -> bool {
        self.log("sealed", name);
        self.state().sealed.contains(name)
    }

    fn pinned(&self, _dir: &OwnedFd, name: &OsStr) -> bool {
        self.log("pinned", name);
        self.state().pinned.contains(name)
    }
}
