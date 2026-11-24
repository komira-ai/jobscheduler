//! The handover file: a request, made by the update hook's `cutover` and `rollback` just before
//! they stop the agent's unit, that the agent's stop hand its runs over to the next process of
//! the agent instead of draining the host.
//!
//! # What it changes
//!
//! A stop request (SIGTERM) normally drains (see the crate's `run` module): the agent lets its
//! runs finish for `--drain-wait`, then stops them. When the stop request finds
//! [`HANDOVER_FILE`] in the state root (`--state-root`), and the agent's runs can be adopted
//! by its next process, it hands over instead: it takes no new job, announces `draining`, waits
//! until no start, stop or post-job check is under way (a start the process left half made
//! would cost the job, a stop would be left unfinished, and a check left owed is run again by
//! the next process at its start, or taints the host when that process cannot run it), and exits with status 0 leaving every run as it is, but for the host-port
//! connections the agent itself carries for a run (`--host-port`), which end with it: the next
//! process offers the ports again. The next process adopts the runs, lists them in its
//! first heartbeat and reports their results.
//!
//! Under an egress ceiling (`--egress allow-list` or `open`) the runs keep their network as it
//! is: the wiring (each run's veth pair, its member of the firewall's set and its forwards) is
//! the kernel's and goes on enforcing the same rules with no agent running; the process that
//! hands over leaves it, together with the run's record in the egress store (the directory
//! `egress` of the state root, written when the run was wired). The next process reads the
//! records at its start and loads its firewall in one transaction that puts back each recorded
//! run's member and forwards, takes over each adopted run's wiring from its record, and
//! removes the wiring and records no run claimed (see the crate's `egress` module).
//!
//! The same file also asks for a handover in place: SIGHUP to the main process, which the update
//! hook sends when both agents list `in-place`, hands the runs over as a stop does and then runs
//! the program again in the same process instead of exiting (see the crate's `run` module), so the
//! unit is never stopped. The process that starts that way removes the file at its start, which
//! is how the hook learns that the request was taken.
//!
//! The file is looked at once, when the stop request is taken; one written later changes
//! nothing for that stop. The agent removes one it finds at its start, after the adoption: a
//! request is for the process that is running when it is made, never for a later one.
//!
//! # Its form
//!
//! Its presence is the request: the agent reads nothing in it ([`HANDOVER_TEXT`] is there for
//! a person). It is written as the hold file is, through [`HANDOVER_TEMP_FILE`] with mode
//! 0600, synced, renamed and the directory synced. It must be a regular file of the agent's
//! user, not a link, that group and others have no permission on; a file that breaks this is
//! not a request (the stop drains, the safe end), and the agent says why.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The name of the handover file in the state root.
pub const HANDOVER_FILE: &str = "handover";

/// The temporary file the handover file is written through, renamed over [`HANDOVER_FILE`].
pub const HANDOVER_TEMP_FILE: &str = "handover.tmp";

/// The text [`write`] puts in the handover file, for a person who finds it.
pub const HANDOVER_TEXT: &str = "the next stop of js-agent leaves its runs to the next process of \
     the agent instead of draining the host; the update hook writes this file and removes it\n";

/// The permission bits of group and others a handover file may not carry.
const FORBIDDEN_MODE: u32 = 0o077;

/// What the state root says of a handover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handover {
    /// There is no handover file: a stop drains.
    Absent,
    /// The file is there and of its form: a stop hands over.
    Asked,
    /// A file is there under the name that breaks the rule of its form, or that cannot be
    /// looked at; the text says why. A stop drains.
    Refused(String),
}

/// The handover of the state root `root`, whose files belong to the user `owner`: the file
/// looked at without following a link.
#[must_use]
pub fn read(root: &Path, owner: u32) -> Handover {
    let meta = match fs::symlink_metadata(root.join(HANDOVER_FILE)) {
        Ok(meta) => meta,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Handover::Absent,
        Err(why) => return Handover::Refused(format!("it cannot be looked at: {why}")),
    };
    if !meta.file_type().is_file() {
        return Handover::Refused("it is not a regular file (a link is not accepted)".to_owned());
    }
    if meta.uid() != owner {
        return Handover::Refused(format!(
            "it is owned by user {}, not by the user running the agent ({owner})",
            meta.uid()
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & FORBIDDEN_MODE != 0 {
        return Handover::Refused(format!(
            "group or others have permission on it (mode {mode:04o})"
        ));
    }
    Handover::Asked
}

/// Writes the handover file of `root`: [`HANDOVER_TEXT`] to [`HANDOVER_TEMP_FILE`] with mode
/// 0600 (set again on a temporary file a crash left), synced, renamed over [`HANDOVER_FILE`],
/// and the directory synced. A file that is there already is written again.
///
/// # Errors
///
/// Any step failed. The file is then what it was before, or the new one: never a part of it.
pub fn write(root: &Path) -> io::Result<()> {
    let temp = root.join(HANDOVER_TEMP_FILE);
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| {
            file.set_permissions(Permissions::from_mode(0o600))
                .and_then(|()| file.write_all(HANDOVER_TEXT.as_bytes()))
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| fs::rename(&temp, root.join(HANDOVER_FILE)))
        .and_then(|()| sync(root))
}

/// Removes the handover file of `root` and syncs the directory; `true` when there was one.
///
/// # Errors
///
/// The file is there and cannot be removed, or the sync failed.
pub fn remove(root: &Path) -> io::Result<bool> {
    match fs::remove_file(root.join(HANDOVER_FILE)) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(why) => Err(why),
        Ok(()) => sync(root).map(|()| true),
    }
}

/// Syncs the directory `root`, so a rename or a removal in it survives a power cut.
fn sync(root: &Path) -> io::Result<()> {
    File::open(root).and_then(|dir| dir.sync_all())
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
