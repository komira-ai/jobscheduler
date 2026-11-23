//! The proof that a deleted account is gone: what a deletion shows before the ledger
//! records `gone`, and so before its uid can be given to anyone again.
//!
//! Each item is looked at after the deletion, in the order of [`Gone`], and the first that
//! does not hold fails the deletion with a [`Step::Proof`] that names it and says what was
//! found. An item that cannot be looked at does not hold either: an error in looking is
//! never read as "gone".

use std::path::PathBuf;

use super::error::Failure;
use super::pool::Pool;
use super::teardown::{NAMED, remaining};
use super::{Gone, Proof, Step, folder, ipc, tree};

/// The failure of the proof item `item`, with `detail`.
fn not_gone(item: Gone, detail: impl std::fmt::Display) -> Failure {
    Failure::new(Step::Proof(item), detail)
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

impl Pool {
    /// Shows that the account `name`, with `uid`, is gone: see the module documentation.
    ///
    /// # Errors
    /// The first item of the proof that does not hold, or cannot be looked at.
    pub(super) fn prove_gone(&self, name: &str, uid: u32) -> Result<Proof, Failure> {
        self.gone_record(name, uid)?;
        self.gone_processes(uid)?;
        self.gone_home(name)?;
        self.gone_folders(uid)?;
        self.gone_places(name, uid)?;
        self.gone_ipc(name, uid)?;
        Ok(Proof::shown())
    }

    /// No record by `name` and none with `uid`, in the directory node the record was deleted
    /// from: that node decides. The other lookups (`getpw*` on macOS) are asked too, and a
    /// lookup of theirs that fails fails the proof, since an error in looking is never an
    /// absence. A record they still answer for while the directory has none is not held
    /// against the deletion, only logged: `getpw*` answers through the system's lookup cache,
    /// which may keep a record for a while after the directory deleted it, and on the proof
    /// Mac retirements were held at "a record named ... remains" while the directory listed
    /// no such user.
    fn gone_record(&self, name: &str, uid: u32) -> Result<(), Failure> {
        let by_name = |why| not_gone(Gone::Record, format!("looking up {name}: {why}"));
        let by_uid = |why| not_gone(Gone::Record, format!("looking up uid {uid}: {why}"));
        if self.host.in_directory(name).map_err(by_name)? {
            return Err(not_gone(
                Gone::Record,
                format!("a record named {name} remains"),
            ));
        }
        if self.host.uid_in_directory(uid).map_err(by_uid)? {
            return Err(not_gone(Gone::Record, format!("a record has uid {uid}")));
        }
        let cached_name = self.host.account(name).map_err(by_name)?.is_some();
        let cached_uid = self.host.uid_taken(uid).map_err(by_uid)?;
        // Both lookups are made and judged, whatever the first answered.
        if cached_name | cached_uid {
            tracing::info!(
                account = %name,
                uid,
                "the directory has no record of the account; a lookup still answers from its cache"
            );
        }
        Ok(())
    }

    /// No process of `uid`. launchd is not asked: the record is deleted by now, and a print
    /// of the uid's user domain would make the domain again for a uid with no account
    /// (`teardown::Launchd`); the kill rounds judged its domains while the record was there.
    fn gone_processes(&self, uid: u32) -> Result<(), Failure> {
        let left = self.host.processes(uid).map_err(|why| {
            not_gone(
                Gone::Processes,
                format!("listing the processes of uid {uid}: {why}"),
            )
        })?;
        match left.is_empty() {
            true => Ok(()),
            false => Err(not_gone(
                Gone::Processes,
                remaining(uid, "after the account was deleted", &left),
            )),
        }
    }

    /// The home folder's path `<homes>/<name>` does not exist.
    fn gone_home(&self, name: &str) -> Result<(), Failure> {
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

    /// Every per-user folder `uid` owns holds nothing below its top-level directories, and
    /// nothing but directories at its top level.
    fn gone_folders(&self, uid: u32) -> Result<(), Failure> {
        let found = folder::left(&self.settings.user_folders, uid, &*self.os).map_err(|why| {
            not_gone(
                Gone::Folders,
                format!("searching for uid {uid}'s per-user folders: {why}"),
            )
        })?;
        match found.is_empty() {
            true => Ok(()),
            false => Err(not_gone(
                Gone::Folders,
                format!("left in uid {uid}'s per-user folders: {}", listed(&found)),
            )),
        }
    }

    /// None of the shared places holds an entry owned by `uid` or named for `name`.
    fn gone_places(&self, name: &str, uid: u32) -> Result<(), Failure> {
        let found =
            tree::left_in(&self.settings.swept_places(), Some(name), uid).map_err(|why| {
                not_gone(Gone::Places, format!("looking in the shared places: {why}"))
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
    fn gone_ipc(&self, name: &str, uid: u32) -> Result<(), Failure> {
        let left = ipc::left(&*self.host, name, uid).map_err(|why| not_gone(Gone::Ipc, why))?;
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
