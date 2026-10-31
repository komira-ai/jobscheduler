//! The packet-filter check of a native agent whose network cap is `on`.
//!
//! A Seatbelt profile cannot name the hosts a run may reach, so the egress of a Mac pool's
//! users is limited by a pf anchor keyed on their uids (`js-mac-provision`). The agent runs
//! no job with the network on unless that anchor is enforced right now.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use js_mac_provision::cli::{DEFAULT_ANCHOR, DEFAULT_STATE_DIR};
use js_mac_provision::pfctl::{SYSTEM_PFCTL, SystemPfctl};
use js_mac_provision::{AnchorName, Config, Endpoint, UidRange, enforced};

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
    /// Checks `flags` given the agent's network cap (`network_on`); `None` when the cap is
    /// below `on` and no pf flag is given.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault: a pf flag with a cap below `on`, a cap of `on`
    /// without the pool's uids, a uid range, anchor name or endpoint the provisioner would
    /// refuse, or a path that is not absolute (the agent runs as root and starts `pfctl` by
    /// exactly this path).
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
            let given = [
                ("--native-pfctl", pfctl.is_some()),
                ("--native-pf-anchor", anchor.is_some()),
                ("--native-pf-state-dir", state_dir.is_some()),
                ("--native-pf-uid-start", uid_start.is_some()),
                ("--native-pf-uid-count", uid_count.is_some()),
                ("--native-pf-endpoint", !endpoints.is_empty()),
            ];
            return match given.iter().find(|(_, given)| *given) {
                Some((flag, _)) => Err(format!("{flag}: needs --native-network on")),
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

impl Enforcer for PfPolicy {
    fn enforced(&self) -> Result<(), ProvisionError> {
        let pf = SystemPfctl::new(&self.pfctl);
        enforced(&Config {
            pf: &pf,
            anchor: &self.anchor,
            dir: &self.state_dir,
            uids: self.uids,
            endpoints: &self.endpoints,
        })
    }
}
