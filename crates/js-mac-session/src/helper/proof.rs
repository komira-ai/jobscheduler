//! The proof that a deleted account is gone: what `user-delete` shows before the ledger
//! records the lease over, and so before its uid can be given to anyone again.
//!
//! Each item is looked at after the deletion, in this order, and the first that does not
//! hold fails the delete with a [`TeardownError::NotGone`] that names it ([`Gone`]) and says
//! what was found. An item that cannot be looked at does not hold either: an error in
//! looking is never read as "gone".
//!
//! 1. [`Gone::Record`]: no user record by the account's name, and none by its uid.
//! 2. [`Gone::Processes`]: no live process of the uid, and the uid's launchd domains read as
//!    gone the way `kill-uid` judges them.
//! 3. [`Gone::Home`]: the home folder's path does not exist (`lstat` answers "no such file or
//!    directory"; a home that exists, or any other answer, is not gone).
//! 4. [`Gone::Folders`]: no directory two levels down in the per-user folders
//!    (`/private/var/folders/<xx>/<hash>`) is owned by the uid.
//! 5. [`Gone::Places`]: none of the places the sweep covers holds an entry owned by the uid
//!    or named for the account ([`crate::sweep::owned_left`]).
//! 6. [`Gone::Ipc`]: a fresh `ipcs -a` lists no System V IPC object that the uid or the
//!    account's name owns or created (always listed again, whether or not an earlier
//!    listing found any: the record is gone now, so the owner reads as the uid's number).

use std::path::{Path, PathBuf};

use super::teardown::{Gone, TeardownError, domains, remaining};
use super::{Helper, NAMED};
use crate::sweep::{owned_entries, owned_left};

/// [`TeardownError::NotGone`] for `what`, with `detail`.
fn not_gone(what: Gone, detail: impl Into<String>) -> TeardownError {
    TeardownError::NotGone {
        what,
        detail: detail.into(),
    }
}

/// `paths`, comma-separated, at most [`NAMED`] of them and then how many more.
fn listed(paths: &[PathBuf]) -> String {
    let mut named: Vec<String> = paths
        .iter()
        .take(NAMED)
        .map(|path| path.display().to_string())
        .collect();
    if paths.len() > NAMED {
        named.push(format!("and {} more", paths.len() - NAMED));
    }
    named.join(", ")
}

impl Helper {
    /// Shows that the account `name`, with `uid`, is gone: see the module documentation.
    ///
    /// # Errors
    /// The first item of the proof that does not hold, or cannot be looked at.
    pub(super) fn prove_gone(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        self.gone_record(name, uid)?;
        self.gone_processes(uid)?;
        self.gone_home(name)?;
        self.gone_folders(uid)?;
        self.gone_places(name, uid)?;
        self.gone_ipc(name, uid)
    }

    /// No record by `name` and none by `uid`.
    fn gone_record(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let account = self
            .host
            .account(name)
            .map_err(|why| not_gone(Gone::Record, format!("looking up {name}: {why}")))?;
        if account.is_some() {
            return Err(not_gone(
                Gone::Record,
                format!("a record named {name} remains"),
            ));
        }
        let taken = self
            .host
            .uid_taken(uid)
            .map_err(|why| not_gone(Gone::Record, format!("looking up uid {uid}: {why}")))?;
        match taken {
            true => Err(not_gone(Gone::Record, format!("a record has uid {uid}"))),
            false => Ok(()),
        }
    }

    /// No process of `uid`, and its launchd domains gone, as `kill-uid` judges them.
    fn gone_processes(&self, uid: u32) -> Result<(), TeardownError> {
        let left = self
            .processes(uid)
            .map_err(|why| not_gone(Gone::Processes, why.to_string()))?;
        if !left.is_empty() {
            return Err(not_gone(
                Gone::Processes,
                remaining(uid, "after the account was deleted", &left),
            ));
        }
        let gui = self
            .gui_owed(uid)
            .map_err(|why| not_gone(Gone::Processes, why.to_string()))?;
        let stayed = self.still_printing(&domains(uid, gui), &[]);
        match stayed.is_empty() {
            true => Ok(()),
            false => Err(not_gone(
                Gone::Processes,
                format!(
                    "launchd domains of uid {uid} remain after the account was deleted: {}",
                    stayed.join(", ")
                ),
            )),
        }
    }

    /// The home folder's path `<homes>/<name>` does not exist.
    fn gone_home(&self, name: &str) -> Result<(), TeardownError> {
        let home = self.settings.homes.join(name);
        match std::fs::symlink_metadata(&home) {
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(not_gone(
                Gone::Home,
                format!("the home {} exists", home.display()),
            )),
            Err(why) => Err(not_gone(
                Gone::Home,
                format!("looking at {}: {why}", home.display()),
            )),
        }
    }

    /// No directory two levels down in the per-user folders is owned by `uid`.
    fn gone_folders(&self, uid: u32) -> Result<(), TeardownError> {
        let folders: &Path = &self.settings.user_folders;
        let found = owned_entries(folders, uid).map_err(|why| {
            not_gone(
                Gone::Folders,
                format!(
                    "searching {} for uid {uid}'s entries: {why}",
                    folders.display()
                ),
            )
        })?;
        match found.is_empty() {
            true => Ok(()),
            false => Err(not_gone(
                Gone::Folders,
                format!("owned by uid {uid}: {}", listed(&found)),
            )),
        }
    }

    /// None of the places the sweep covers holds an entry owned by `uid` or named for `name`.
    fn gone_places(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let found = owned_left(&self.swept_places(), Some(name), uid).map_err(|why| {
            not_gone(
                Gone::Places,
                format!("looking in the places the sweep covers: {why}"),
            )
        })?;
        match found.is_empty() {
            true => Ok(()),
            false => Err(not_gone(
                Gone::Places,
                format!("left of uid {uid}'s or {name}'s: {}", listed(&found)),
            )),
        }
    }

    /// No System V IPC object of `uid` or `name`, in a fresh listing.
    fn gone_ipc(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let left = self
            .ipc_left(name, uid)
            .map_err(|why| not_gone(Gone::Ipc, why.to_string()))?;
        match left.is_empty() {
            true => Ok(()),
            false => Err(not_gone(
                Gone::Ipc,
                format!(
                    "IPC objects of uid {uid} remain after the account was deleted: {}",
                    left.join(", ")
                ),
            )),
        }
    }
}

#[cfg(test)]
mod tests;
