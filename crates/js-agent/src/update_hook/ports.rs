//! What the hook's steps do to the machine, as ports: the unit manager, the release link, the
//! lease server's own-host routes, the agent's state root (and its hold file, which `drain`
//! writes and `resume` removes), the host facts `resume` compares, and time.
//!
//! Each port is a small trait with the machine's implementation (or, for the lease server, with
//! the real client in a later change) and a fake beside the tests that keeps its world in a
//! directory, so that a second process sees it. The steps take a [`Ports`] and never reach for
//! the machine themselves. The unit manager, the release link, time and the kill points are
//! js-update-hook's, shared with every hook; the lease server and the state root are the
//! agent's own.
//!
//! Nothing here reads an environment variable. [`Gate`] is how the test binary's fakes tell the
//! conformance harness that a change is about to be made; on the machine it does nothing.

use js_agent_wire::Taint;
pub use js_update_hook::{
    Clock, Gate, Link, NoGate, PortError, SYSTEMCTL, SystemClock, SystemLink, SystemUnits,
    UnitState, Units, read_link,
};

use crate::lifecycle::StateRoot;
use crate::lifecycle::handover;
use crate::lifecycle::hold::{self, HOLD_FILE, HOLD_RESUME_FILE};

// The lease server's own-host routes are not the hook's alone: `js-agent cordon` calls them too,
// on macOS as well, so their port lives in `crate::own_host`.
pub use crate::own_host::{
    CordonView, Cordoned, HostReading, Lease, LeaseError, MAX_CORDON_SECS, Uncordoned,
};

/// The agent's state root, read only: the taint marker and pin file, the records of owed checks
/// and the handover request.
///
/// The agent runs as root on the crun unit, so the root hook can open its state root.
pub trait StateView {
    /// The taint on record (an unreadable marker counts as a taint).
    fn taint(&self) -> Option<Taint>;

    /// Whether a restarted agent re-checks `taint`, the taint on record, and so may clear it
    /// itself: a residue of a post-job check that the state root's pin file does not name. This is
    /// the rule the agent applies when it starts ([`StateRoot::restored_rechecks`]).
    fn restored_rechecks(&self, taint: &Taint) -> bool;

    /// The runs whose post-job checks are owed.
    ///
    /// # Errors
    /// The state root could not be listed.
    fn owed(&self) -> Result<Vec<String>, PortError>;

    /// Asks the agent's next stop to hand its runs over to the agent's next process instead
    /// of draining the host: writes the handover file in the state root, atomically and
    /// durably (see [`crate::lifecycle::handover`]).
    ///
    /// # Errors
    /// The file could not be written.
    fn request_handover(&self) -> Result<(), PortError>;

    /// Withdraws the request: removes the handover file, if it is there.
    ///
    /// # Errors
    /// The file is there and could not be removed.
    fn withdraw_handover(&self) -> Result<(), PortError>;

    /// Whether the handover file is still there: the agent removes it once it has taken the
    /// request, after it has adopted the runs.
    ///
    /// # Errors
    /// The file could not be looked at (a missing file is no error).
    fn handover_pending(&self) -> Result<bool, PortError>;
}

impl StateView for StateRoot {
    fn taint(&self) -> Option<Taint> {
        self.read()
    }

    fn restored_rechecks(&self, taint: &Taint) -> bool {
        StateRoot::restored_rechecks(self, taint)
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        StateRoot::owed(self).map_err(PortError::from)
    }

    fn request_handover(&self) -> Result<(), PortError> {
        handover::write(self.path()).map_err(PortError::from)
    }

    fn withdraw_handover(&self) -> Result<(), PortError> {
        handover::remove(self.path())
            .map(|_| ())
            .map_err(PortError::from)
    }

    fn handover_pending(&self) -> Result<bool, PortError> {
        match std::fs::symlink_metadata(self.path().join(handover::HANDOVER_FILE)) {
            Ok(_) => Ok(true),
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(why) => Err(PortError::from(why)),
        }
    }
}

/// What the agent's state root says of the hold, as `drain` and `resume` read it (see
/// [`crate::lifecycle::hold`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldState {
    /// Neither the hold file nor the one a `resume` renamed it to is there: the host is not
    /// held.
    Free,
    /// The hold file is there, with this text.
    Held(String),
    /// The hold file was renamed by a step that puts the host back and did not finish: the
    /// cordon may still be in force. The renamed file holds this text.
    Releasing(String),
}

/// The hold file of the agent's state root: `drain` writes it, and `resume` ends it.
pub trait Holds {
    /// What the state root says of the hold.
    ///
    /// # Errors
    /// A file is there under either name that is not a regular file, or cannot be read.
    fn state(&self) -> Result<HoldState, PortError>;

    /// Writes the hold file with `text`, atomically and durably.
    ///
    /// # Errors
    /// The file could not be written; the hold is then as it was.
    fn hold(&self, text: &str) -> Result<(), PortError>;

    /// Renames the hold file for the end of the hold (see [`HoldState::Releasing`]).
    ///
    /// # Errors
    /// There is no hold file, or the rename failed.
    fn release(&self) -> Result<(), PortError>;

    /// Removes the renamed hold file, if it is there: the hold is over.
    ///
    /// # Errors
    /// The file is there and could not be removed.
    fn released(&self) -> Result<(), PortError>;
}

impl Holds for StateRoot {
    fn state(&self) -> Result<HoldState, PortError> {
        if let Some(text) = hold::read_text(self, HOLD_FILE)? {
            return Ok(HoldState::Held(text));
        }
        let renamed = hold::read_text(self, HOLD_RESUME_FILE)?;
        Ok(renamed.map_or(HoldState::Free, HoldState::Releasing))
    }

    fn hold(&self, text: &str) -> Result<(), PortError> {
        Ok(hold::write_text(self, text)?)
    }

    fn release(&self) -> Result<(), PortError> {
        Ok(hold::release(self)?)
    }

    fn released(&self) -> Result<(), PortError> {
        Ok(hold::released(self)?)
    }
}

/// The host facts the agent reads when it starts and says hello with, which work on the host
/// besides an update can change (a new major version of macOS, another GPU): the agent reads
/// them again only when it starts.
pub trait Facts {
    /// The facts, as one line of text: equal facts give equal lines.
    ///
    /// # Errors
    /// A fact cannot be read; the text says which.
    fn read(&self) -> Result<String, String>;
}

/// Everything a step may touch.
pub struct Ports<'a> {
    /// The unit manager.
    pub units: &'a dyn Units,
    /// The release link.
    pub link: &'a dyn Link,
    /// The lease server.
    pub lease: &'a dyn Lease,
    /// The agent's state root.
    pub state: &'a dyn StateView,
    /// Time.
    pub clock: &'a dyn Clock,
    /// Kill points.
    pub gate: &'a dyn Gate,
}

impl Ports<'_> {
    /// A kill point: a no-op on the machine.
    pub fn point(&self) {
        self.gate.point();
    }
}
