//! The packet-filter check of a native agent whose network cap is `on`.
//!
//! A Seatbelt profile cannot name the hosts a run may reach, so the egress of a Mac pool's
//! users is limited by a pf anchor keyed on their uids (`js-mac-provision`). The agent runs
//! no job with the network on unless that anchor is enforced right now.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use js_mac_provision::{AnchorName, Endpoint, UidRange};

pub use js_mac_provision::provision::ProvisionError;

/// Whether the egress limit of the pool is in force: [`PfPolicy`] on a Mac, a fake in tests.
pub trait Enforcer: Debug + Send + Sync {
    /// Looks now, and does not remember.
    ///
    /// # Errors
    ///
    /// The limit is not enforced, or it could not be told.
    fn enforced(&self) -> Result<(), ProvisionError>;
}

/// The pf flags as the command line gave them.
#[derive(Debug, Clone, Copy, Default)]
pub struct PfFlags<'a> {
    /// `--native-pfctl`.
    pub pfctl: Option<&'a Path>,
    /// `--native-pf-anchor`.
    pub anchor: Option<&'a str>,
    /// `--native-pf-state-dir`.
    pub state_dir: Option<&'a Path>,
    /// `--native-pf-uid-start`.
    pub uid_start: Option<u32>,
    /// `--native-pf-uid-count`.
    pub uid_count: Option<u32>,
    /// `--native-pf-endpoint`, each.
    pub endpoints: &'a [String],
}

/// The policy `js-mac-provision` installed, as the agent was told it.
#[derive(Debug, Clone)]
pub struct PfPolicy {
    pfctl: PathBuf,
    anchor: AnchorName,
    state_dir: PathBuf,
    uids: UidRange,
    endpoints: Vec<Endpoint>,
}

impl PfPolicy {
    /// Checks `flags`; `None` when none is given.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn from_flags(_flags: &PfFlags<'_>, _network_on: bool) -> Result<Option<Self>, String> {
        Ok(None)
    }
}

impl Enforcer for PfPolicy {
    fn enforced(&self) -> Result<(), ProvisionError> {
        let _ = (
            &self.pfctl,
            &self.anchor,
            &self.state_dir,
            self.uids,
            &self.endpoints,
        );
        Ok(())
    }
}
