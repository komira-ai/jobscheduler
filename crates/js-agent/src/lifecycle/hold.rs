//! The hold file: the record, kept across restarts and reboots, that this host is out of
//! service until a later step puts it back (the drain of an update: hostkeeper's
//! `docs/design/maintenance-updates.md`, sections 5.3 and 5.4).
//!
//! # The file
//!
//! One file, [`HOLD_FILE`], in the state root (`--state-root`), beside the taint marker. Its
//! presence is the hold: the agent reads nothing in it, and the text [`write`] puts there is
//! for a person. While it is there the agent
//!
//! - starts no run and no job: every `Start` and every job start is answered
//!   `failed_to_start` with the code `host-held` (`js_agent_wire::refusal::HOST_HELD`),
//!   which, unlike `host-tainted`, does not taint the host. The file is looked at for each
//!   start, so a hold written while the agent runs refuses the next start, and one removed
//!   lets the next start run;
//! - cordons the host through the lease server's own-host route as soon as a hello of the
//!   process has made the host known to the server, and renews the cordon before it lapses
//!   (see the `hold_cordon` module of the crate).
//!
//! The agent never writes or removes the file: the step that drains the host writes it with
//! [`write`], and only the step that puts the host back removes it.
//!
//! # Its form
//!
//! - **It is written atomically and durably**, as the taint marker is: the text goes to
//!   [`HOLD_TEMP_FILE`] in the same directory with mode 0600, the file is synced, renamed
//!   over the hold file, and the directory is synced. A crash leaves the hold there or not,
//!   never a part of it, and a hold that was written survives a power cut.
//! - **It must be a regular file of the agent's user that group and others have no
//!   permission on**, the rule of the state root itself. It is looked at without following
//!   a link. A file that breaks the rule is refused as a hold (the agent says why, in the
//!   refusal of each start and in its log), and still holds the host: a file that is there
//!   is never taken for an absent one, as a taint marker that cannot be read is still a
//!   taint. Only its removal ends the hold.
//! - **It belongs to the state root, not to the machine**, as the taint marker does: an
//!   agent started with another `--state-root`, or a state root on a file system lost at a
//!   reboot, does not know of it.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::taint::StateRoot;

/// The name of the hold file in the state root.
pub const HOLD_FILE: &str = "hold";

/// The temporary file the hold file is written through, renamed over [`HOLD_FILE`].
pub const HOLD_TEMP_FILE: &str = "hold.tmp";

/// The text [`write`] puts in the hold file, for a person who finds it.
pub const HOLD_TEXT: &str =
    "js-agent starts no work on this host and keeps it cordoned while this file is here\n";

/// The permission bits of group and others a hold file may not carry.
const FORBIDDEN_MODE: u32 = 0o077;

/// What the state root says of the hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// There is no hold file: the host is not held.
    Absent,
    /// The hold file is there and is of the form the module documentation gives: the host
    /// is held.
    Held,
    /// A file is there under the hold file's name that breaks the rule of its form, or
    /// that cannot be looked at; the text says why. The host is held all the same.
    Refused(String),
}

impl Hold {
    /// Whether the host is held: a hold file is there, refused or not.
    #[must_use]
    pub fn holds(&self) -> bool {
        !matches!(self, Hold::Absent)
    }

    /// Why a start is refused while the host is held, for the refusal's text and the log;
    /// `None` when it is not held.
    #[must_use]
    pub fn why(&self) -> Option<String> {
        match self {
            Hold::Absent => None,
            Hold::Held => Some("this host is held (its hold file is there)".to_owned()),
            Hold::Refused(why) => Some(format!(
                "this host is held: its hold file is refused ({why}) and holds it until it is \
                 removed"
            )),
        }
    }
}

/// The hold of the state root `root`, whose files belong to the user `owner`: the hold file
/// looked at without following a link.
#[must_use]
pub fn read(root: &StateRoot, owner: u32) -> Hold {
    let meta = match fs::symlink_metadata(root.path().join(HOLD_FILE)) {
        Ok(meta) => meta,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Hold::Absent,
        Err(why) => return Hold::Refused(format!("it cannot be looked at: {why}")),
    };
    if !meta.file_type().is_file() {
        return Hold::Refused("it is not a regular file (a link is not accepted)".to_owned());
    }
    if meta.uid() != owner {
        return Hold::Refused(format!(
            "it is owned by user {}, not by the user running the agent ({owner})",
            meta.uid()
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & FORBIDDEN_MODE != 0 {
        return Hold::Refused(format!(
            "group or others have permission on it (mode {mode:04o})"
        ));
    }
    Hold::Held
}

/// The hold of `root` for the user the process runs as: [`read`] with the effective user id,
/// the owner the state root itself was checked for when it was opened.
#[must_use]
pub fn read_own(root: &StateRoot) -> Hold {
    read(root, rustix::process::geteuid().as_raw())
}

/// Writes the hold file of `root`: [`HOLD_TEXT`] to [`HOLD_TEMP_FILE`] with mode 0600 (set
/// again on a temporary file a crash left), synced, renamed over [`HOLD_FILE`], and the
/// directory synced. A hold that is there already is written again.
///
/// # Errors
///
/// Any step failed. The hold file is then what it was before, or the new one: never a part
/// of it.
pub fn write(root: &StateRoot) -> io::Result<()> {
    let temp = root.path().join(HOLD_TEMP_FILE);
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| {
            file.set_permissions(Permissions::from_mode(0o600))
                .and_then(|()| file.write_all(HOLD_TEXT.as_bytes()))
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| fs::rename(&temp, root.path().join(HOLD_FILE)))
        .and_then(|()| File::open(root.path()).and_then(|dir| dir.sync_all()))
}
