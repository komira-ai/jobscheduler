//! What the hook's steps do to the machine, as ports: the unit manager, the release link, the
//! lease server's own-host routes, the agent's state root, and time.
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
    UnitState, Units, read_link, swap_link,
};

use crate::lifecycle::StateRoot;

// The lease server's own-host routes are not the hook's alone: `js-agent cordon` calls them too,
// on macOS as well, so their port lives in `crate::own_host`.
pub use crate::own_host::{
    CordonView, Cordoned, HostReading, Lease, LeaseError, MAX_CORDON_SECS, Uncordoned,
};

/// The agent's state root, read only: the taint marker and the records of owed checks.
///
/// The agent runs as root on the crun unit, so the root hook can open its state root.
pub trait StateView {
    /// The taint on record (an unreadable marker counts as a taint).
    fn taint(&self) -> Option<Taint>;

    /// The runs whose post-job checks are owed.
    ///
    /// # Errors
    /// The state root could not be listed.
    fn owed(&self) -> Result<Vec<String>, PortError>;
}

impl StateView for StateRoot {
    fn taint(&self) -> Option<Taint> {
        self.read()
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        StateRoot::owed(self).map_err(PortError::from)
    }
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
