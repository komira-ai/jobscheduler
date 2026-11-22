//! Whether a process holds Full Disk Access, the privacy (TCC) service
//! `SystemPolicyAllFiles`: the judgement `js-agent` makes of its own listing at start, and
//! the helper of its own for the verb `full-disk-access`.
//!
//! macOS gives the grant to a program by a privacy profile that device management
//! installs, and gives no call to ask whether a program has it. A program finds out the
//! way every program does: it lists the directory of the privacy database, which only a
//! process with Full Disk Access may list. The listing opens no file and reads no
//! database. The profile grants it to `js-agent` alone, not to the helper, and everything
//! either starts is started with its privacy responsibility disclaimed
//! ([`crate::disclaim`]), so the helper's grants never reach anything; a job's start process
//! (the agent's own program) is disclaimed too and executes the job with the responsibility
//! kept, so the agent's grant reaches the job, on purpose.

use std::io;

use serde::{Deserialize, Serialize};

/// The directory of the privacy database, which only a process with Full Disk Access may
/// list.
pub const PRIVACY_DIR: &str = "/Library/Application Support/com.apple.TCC";

/// Whether a process holds Full Disk Access (the privacy service `SystemPolicyAllFiles`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FullDiskAccess {
    /// The directory of the privacy database could be listed: the grant is held.
    Granted,
    /// macOS refused the listing: the grant is not held.
    NotGranted,
    /// The listing failed for another reason (the directory is missing, say), or the
    /// answer was not asked for: nothing is known.
    Undetermined,
}

impl FullDiskAccess {
    /// The state as the agent reports it in its capabilities: `granted`, `not-granted`
    /// or `undetermined`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::NotGranted => "not-granted",
            Self::Undetermined => "undetermined",
        }
    }
}

/// The state a listing of [`PRIVACY_DIR`] shows: a listing that worked is
/// [`FullDiskAccess::Granted`], one that was refused for want of permission is
/// [`FullDiskAccess::NotGranted`], and any other failure is
/// [`FullDiskAccess::Undetermined`], returned with the error for the log.
#[must_use]
pub fn judge(listed: io::Result<()>) -> (FullDiskAccess, Option<io::Error>) {
    match listed {
        Ok(()) => (FullDiskAccess::Granted, None),
        Err(why) if why.kind() == io::ErrorKind::PermissionDenied => {
            (FullDiskAccess::NotGranted, None)
        }
        Err(why) => (FullDiskAccess::Undetermined, Some(why)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a listing that worked is a grant, a permission refusal is no grant
    /// with nothing to log, and any other failure (a missing directory, an I/O error)
    /// is undetermined and keeps its error. Catches: a missing directory read as a
    /// refusal (a host where the check cannot work would then look like one that
    /// lacks the grant), or a refusal read as unknown.
    #[test]
    fn a_listing_is_judged_by_how_it_ended() {
        assert_eq!(judge(Ok(())).0, FullDiskAccess::Granted);
        assert!(judge(Ok(())).1.is_none());
        let (state, why) = judge(Err(io::ErrorKind::PermissionDenied.into()));
        assert_eq!(state, FullDiskAccess::NotGranted);
        assert!(why.is_none());
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::Other] {
            let (state, why) = judge(Err(kind.into()));
            assert_eq!(state, FullDiskAccess::Undetermined);
            assert_eq!(why.map(|why| why.kind()), Some(kind));
        }
    }

    /// Asserts that each state's text is the one the agent reports and that the wire
    /// spelling of the state is the same text. Catches: a state renamed in one place
    /// only, which the agent's capability would then show differently from the helper's
    /// reply.
    #[test]
    fn each_state_has_one_spelling() {
        for (state, text) in [
            (FullDiskAccess::Granted, "granted"),
            (FullDiskAccess::NotGranted, "not-granted"),
            (FullDiskAccess::Undetermined, "undetermined"),
        ] {
            assert_eq!(state.as_str(), text);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{text}\"")
            );
        }
    }
}
