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
//!
//! # The process id it leaves
//!
//! A process that hands its runs over leaves its agent process id (the one every hello of it
//! names, `js_agent_wire::Hello::agent_process`) in `PROCESS_FILE` (`handover-process`) beside
//! the handover file (`leave_process`), and the next process takes it at its start
//! (`take_process`: read once and removed) and says hello as that process. The server holds the host live in the session
//! of that id until its grace runs out, and takes a hello naming the same process at once
//! instead of refusing it `host-live`; so the host is not seen lost while the agent hands over
//! when the next process says hello within the grace of the last heartbeat (a hand-over in place
//! takes seconds). Nothing else is held: the server hears the host only from the heartbeats and
//! hellos it is sent, and a host whose next process never says hello is lost once the grace after
//! the last heartbeat has passed, as any silent host is.
//!
//! The file is written as the handover file is (through `PROCESS_TEMP_FILE`, mode 0600) and
//! holds the id and a newline. Its form is the handover file's: a file that is a link, of
//! another user or open to group or others, or one that cannot be read or removed, is not used,
//! and the next process draws an id of its own and waits out the grace as before. An agent from
//! before the file neither writes nor reads it.

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

/// The name of the file in the state root a process that hands its runs over leaves its agent
/// process id in, for the next process to say hello as.
pub(crate) const PROCESS_FILE: &str = "handover-process";

/// The temporary file the process file is written through, renamed over [`PROCESS_FILE`].
pub(crate) const PROCESS_TEMP_FILE: &str = "handover-process.tmp";

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
    match fs::symlink_metadata(root.join(HANDOVER_FILE)) {
        Ok(meta) => match of_form(&meta, owner) {
            Ok(()) => Handover::Asked,
            Err(why) => Handover::Refused(why),
        },
        Err(why) if why.kind() == io::ErrorKind::NotFound => Handover::Absent,
        Err(why) => Handover::Refused(format!("it cannot be looked at: {why}")),
    }
}

/// Whether `meta`, of a file looked at without following a link, is of the form of the files
/// the update writes in a state root of the user `owner`; the error says why not.
fn of_form(meta: &fs::Metadata, owner: u32) -> Result<(), String> {
    if !meta.file_type().is_file() {
        return Err("it is not a regular file (a link is not accepted)".to_owned());
    }
    if meta.uid() != owner {
        return Err(format!(
            "it is owned by user {}, not by the user running the agent ({owner})",
            meta.uid()
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & FORBIDDEN_MODE != 0 {
        return Err(format!(
            "group or others have permission on it (mode {mode:04o})"
        ));
    }
    Ok(())
}

/// Writes the handover file of `root`: [`HANDOVER_TEXT`] to [`HANDOVER_TEMP_FILE`] with mode
/// 0600 (set again on a temporary file a crash left), synced, renamed over [`HANDOVER_FILE`],
/// and the directory synced. A file that is there already is written again.
///
/// # Errors
///
/// Any step failed. The file is then what it was before, or the new one: never a part of it.
pub fn write(root: &Path) -> io::Result<()> {
    write_through(root, (HANDOVER_TEMP_FILE, HANDOVER_FILE), HANDOVER_TEXT)
}

/// Leaves the agent process id `id` in the process file of `root` for the next process (see
/// the module documentation), written as [`write`] writes the handover file.
///
/// # Errors
///
/// Any step failed. The file is then what it was before, or the new one: never a part of it.
pub(crate) fn leave_process(root: &Path, id: &str) -> io::Result<()> {
    write_through(root, (PROCESS_TEMP_FILE, PROCESS_FILE), &format!("{id}\n"))
}

/// What the state root holds of an agent process id the last hand-over left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Left {
    /// There is no process file.
    Absent,
    /// The process file was of its form and read, and is removed: its text, the newline
    /// that ends it taken off.
    Kept(String),
    /// A file was there under the name and is not used; the text says why.
    Refused(String),
}

/// Takes the process file of `root`, whose files belong to the user `owner`: looked at without
/// following a link, read when it is of its form, and removed whatever it held (a link itself,
/// not what it names), so that it is used once. A file that cannot be removed is not used.
pub(crate) fn take_process(root: &Path, owner: u32) -> Left {
    let path = root.join(PROCESS_FILE);
    let found = match fs::symlink_metadata(&path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Left::Absent,
        found => found,
    };
    let judged = found
        .map_err(|why| format!("it cannot be looked at: {why}"))
        .and_then(|meta| of_form(&meta, owner))
        .and_then(|()| {
            fs::read_to_string(&path).map_err(|why| format!("it cannot be read: {why}"))
        });
    let removed = fs::remove_file(&path).and_then(|()| sync(root));
    match (judged, removed) {
        (_, Err(why)) => Left::Refused(format!("it cannot be removed: {why}")),
        (Err(why), Ok(())) => Left::Refused(why),
        (Ok(text), Ok(())) => Left::Kept(text.trim_end_matches('\n').to_owned()),
    }
}

/// Writes `text` to the file named `names.1` in `root` through the temporary file `names.0`:
/// mode 0600 (set again on a temporary file a crash left), synced, renamed, and the directory
/// synced.
fn write_through(root: &Path, names: (&str, &str), text: &str) -> io::Result<()> {
    let (temp, name) = (root.join(names.0), root.join(names.1));
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| {
            file.set_permissions(Permissions::from_mode(0o600))
                .and_then(|()| file.write_all(text.as_bytes()))
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| fs::rename(&temp, name))
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
