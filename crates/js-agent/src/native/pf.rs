//! The packet-filter check of a native agent whose network ceiling is `on`. A ceiling of
//! `open` has no filter and takes none of these flags.
//!
//! A Seatbelt profile cannot name the hosts a run may reach, so the egress of a Mac pool's
//! users is limited by a pf anchor keyed on their uids (`js-mac-provision`). The agent runs
//! no job with the network on unless that anchor is enforced right now.
//!
//! The agent loads the anchor itself: at its start, before the check it makes then (when
//! that first load is refused it waits one tick and loads once more), and every 30 seconds
//! after that on a thread of its own (`js_mac_provision::keep`). The check is made at the
//! agent's start, after that load, and at every admission; it only checks, never loads. The
//! window from admission to the helper's `run` (the slot, the user, the launch) is not
//! checked again, and a run that is under way is not checked again after a reload of the pf
//! ruleset.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_mac_provision::cli::{DEFAULT_ANCHOR, DEFAULT_STATE_DIR};
use js_mac_provision::keep::{self, Every, INTERVAL, Keeper};
use js_mac_provision::pfctl::{SYSTEM_PFCTL, SystemPfctl};
use js_mac_provision::{AnchorName, Config, Endpoint, UidRange, apply, enforced};

use super::{Ceiling, RuntimeError, fail};

pub use js_mac_provision::keep::{Applied, Ticks};
pub use js_mac_provision::provision::ProvisionError;

/// Whether the egress limit of the pool is in force: [`PfPolicy`] on a Mac, a fake in tests.
pub trait Enforcer: Debug + Send + Sync {
    /// Looks now, and does not remember.
    ///
    /// # Errors
    ///
    /// The limit is not enforced, or it could not be told.
    fn enforced(&self) -> Result<(), ProvisionError>;

    /// Loads the anchor unless it is in force.
    ///
    /// # Errors
    ///
    /// The anchor could not be loaded.
    fn apply(&self) -> Result<Applied, ProvisionError>;

    /// The clock of the keeper that loads the anchor again: every 30 seconds for the real
    /// policy; a test gives its own.
    fn ticks(&self) -> Box<dyn Ticks>;
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

impl PfFlags<'_> {
    /// The first pf flag the command line gave, by its name, or `None` when it gave none.
    #[must_use]
    pub(crate) fn first_given(&self) -> Option<&'static str> {
        let given = [
            ("--native-pfctl", self.pfctl.is_some()),
            ("--native-pf-anchor", self.anchor.is_some()),
            ("--native-pf-state-dir", self.state_dir.is_some()),
            ("--native-pf-uid-start", self.uid_start.is_some()),
            ("--native-pf-uid-count", self.uid_count.is_some()),
            ("--native-pf-endpoint", !self.endpoints.is_empty()),
        ];
        given
            .iter()
            .find(|(_, given)| *given)
            .map(|(flag, _)| *flag)
    }
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
    /// Checks `flags` given whether the agent's network ceiling is `on` (`network_on`); `None`
    /// when it is not and no pf flag is given. A pf flag beside a ceiling of `open` is refused
    /// before this, naming `--native-network open` (`NativeSettings::from_flags`).
    ///
    /// # Errors
    ///
    /// The text names the flag at fault: a pf flag with a ceiling that is not `on`, a ceiling
    /// of `on` without the pool's uids, a uid range, anchor name or endpoint the provisioner
    /// would refuse, or a path that is not absolute (the agent runs as root and starts
    /// `pfctl` by exactly this path).
    pub fn from_flags(flags: &PfFlags<'_>, network_on: bool) -> Result<Option<Self>, String> {
        let PfFlags {
            pfctl,
            anchor,
            state_dir,
            uid_start,
            uid_count,
            endpoints,
        } = *flags;
        if !network_on {
            return match flags.first_given() {
                Some(flag) => Err(format!("{flag}: needs --native-network on")),
                None => Ok(None),
            };
        }
        let required = |flag: &str, value: Option<u32>| {
            value.ok_or_else(|| {
                format!(
                    "{flag}: required with --native-network on: the uids of the pool whose \
                     egress the pf anchor limits"
                )
            })
        };
        let start = required("--native-pf-uid-start", uid_start)?;
        let count = required("--native-pf-uid-count", uid_count)?;
        let uids = UidRange::new(start, count)
            .map_err(|why| format!("--native-pf-uid-start and --native-pf-uid-count: {why}"))?;
        let anchor = AnchorName::new(anchor.unwrap_or(DEFAULT_ANCHOR))
            .map_err(|why| format!("--native-pf-anchor: {why}"))?;
        let endpoints = endpoints
            .iter()
            .map(|endpoint| {
                endpoint
                    .parse::<Endpoint>()
                    .map_err(|why| format!("--native-pf-endpoint: {endpoint}: {why}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let pfctl = pfctl.unwrap_or_else(|| Path::new(SYSTEM_PFCTL));
        let state_dir = state_dir.unwrap_or_else(|| Path::new(DEFAULT_STATE_DIR));
        for (flag, path) in [
            ("--native-pfctl", pfctl),
            ("--native-pf-state-dir", state_dir),
        ] {
            if !path.is_absolute() {
                return Err(format!(
                    "{flag}: {} is not an absolute path",
                    path.display()
                ));
            }
        }
        Ok(Some(Self {
            pfctl: pfctl.to_path_buf(),
            anchor,
            state_dir: state_dir.to_path_buf(),
            uids,
            endpoints,
        }))
    }
}

impl PfPolicy {
    /// Runs `f` over the config of this policy, with its `pfctl`.
    fn with_config<T>(&self, f: impl FnOnce(&Config<'_>) -> T) -> T {
        let pf = SystemPfctl::new(&self.pfctl);
        f(&Config {
            pf: &pf,
            anchor: &self.anchor,
            dir: &self.state_dir,
            uids: self.uids,
            endpoints: &self.endpoints,
        })
    }
}

impl Enforcer for PfPolicy {
    fn enforced(&self) -> Result<(), ProvisionError> {
        self.with_config(enforced)
    }

    fn apply(&self) -> Result<Applied, ProvisionError> {
        self.with_config(apply)
    }

    fn ticks(&self) -> Box<dyn Ticks> {
        Box::new(Every(INTERVAL))
    }
}

/// Starts the keeper of `egress` at the agent's start: the first load (made once more on the
/// next tick when it is refused), then a load at every tick on a thread of its own, until the
/// returned [`Keeper`] is dropped. A refused first load does not fail the start by itself: the
/// check that follows decides. Each load that installed is logged, and each that failed.
pub(super) fn keep(egress: &Arc<dyn Enforcer>) -> Keeper {
    let check = Arc::clone(egress);
    let apply = move || {
        let applied = check.apply();
        match &applied {
            Ok(Applied::Installed) => tracing::info!("the pf anchor was loaded"),
            Ok(Applied::InForce) => {}
            Err(why) => tracing::warn!("the pf anchor could not be loaded: {why}"),
        }
        applied
    };
    keep::start(Box::new(apply), egress.ticks()).1
}

/// Whether the pf anchor of the pool is in force right now, when the ceiling `network` is `on`
/// (the one value whose jobs the anchor limits; an `open` ceiling has no filter to check),
/// by `egress`. Asked afresh at every call: a reload of the main ruleset can drop the anchor
/// at any time, so no answer is kept. The text is the first line of what the provisioner said,
/// with the flag that makes the check necessary. It only checks, never loads.
pub(super) fn in_force(
    network: Ceiling,
    egress: Option<&dyn Enforcer>,
) -> Result<(), RuntimeError> {
    if network != Ceiling::On {
        return Ok(());
    }
    // Fails closed: `open` refuses a cap of `on` with no check, and this does not rely on it.
    let Some(check) = egress else {
        return Err(fail(
            &"--native-network",
            &"on has no check of the pf anchor of the pool",
        ));
    };
    check.enforced().map_err(|why| {
        tracing::warn!("the pf anchor is not in force: {why}");
        let said = why.to_string();
        fail(
            &"--native-network",
            &format!(
                "on needs the pf anchor of the pool in force: {}",
                said.lines().next().unwrap_or_default()
            ),
        )
    })
}
