//! What the hook's steps do to the machine, as ports: the unit manager, the release link, the
//! lease server's own-host routes, the agent's state root, and time.
//!
//! Each port is a small trait with the machine's implementation here (or, for the lease
//! server, with the real client in a later change) and a fake beside the tests that keeps its
//! world in a directory, so that a second process sees it. The steps take a [`Ports`] and never
//! reach for the machine themselves.
//!
//! Nothing here reads an environment variable. [`Gate`] is how the test binary's fakes tell the
//! conformance harness that a change is about to be made; on the machine it does nothing.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::Taint;
use serde::{Deserialize, Serialize};

use crate::facts::CommandRunner;
use crate::lifecycle::StateRoot;

/// The unit manager's client, by the absolute path it has on a stock system.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// The longest cordon the lease server grants in one request; a longer ask is cut to it.
pub const MAX_CORDON_SECS: u64 = 3600;

/// Why a port could not do what was asked of it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PortError {
    /// A program could not be run, or exited with a failure.
    #[error("{0}")]
    Command(String),
    /// A file could not be read or written.
    #[error("{0}")]
    Io(String),
    /// A program printed what could not be understood.
    #[error("{0}")]
    Parse(String),
}

impl From<io::Error> for PortError {
    fn from(err: io::Error) -> Self {
        PortError::Io(err.to_string())
    }
}

/// What the unit manager says of a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitState {
    /// `ActiveState`: `active`, `inactive`, `deactivating`, `failed`, ...
    pub active: String,
    /// `SubState`: `running`, `dead`, `stop-sigterm`, ...
    pub sub: String,
}

/// The unit manager.
pub trait Units {
    /// `ActiveState` and `SubState` of `unit`.
    ///
    /// # Errors
    /// The program failed or printed no such properties.
    fn show(&self, unit: &str) -> Result<UnitState, PortError>;

    /// Asks `unit` to stop and returns without waiting for it to.
    ///
    /// # Errors
    /// The program failed.
    fn stop_no_block(&self, unit: &str) -> Result<(), PortError>;

    /// Starts `unit` and returns when the manager has queued the start.
    ///
    /// # Errors
    /// The program failed.
    fn start(&self, unit: &str) -> Result<(), PortError>;

    /// Whether `unit` is active. `systemctl is-active` exits non-zero for every other state, and
    /// a failure to run it looks the same: a caller that must tell the two apart uses
    /// [`Units::show`].
    ///
    /// # Errors
    /// The fake's world could not be read; the machine's answer is never an error.
    fn is_active(&self, unit: &str) -> Result<bool, PortError>;
}

/// [`Units`] over `systemctl`. The unit comes after `--`, so a name that begins with a hyphen is
/// not read as an option.
pub struct SystemUnits<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> SystemUnits<'a> {
    /// Runs `systemctl` through `runner`.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }
}

/// The value of `key` in `systemctl show` output (`Key=value` lines, in the manager's order).
/// A property printed twice, or with an empty value, is refused: the manager prints each of
/// these once and always with a state, so either is not its output.
fn property(text: &str, key: &str) -> Result<String, PortError> {
    let mut values = text
        .lines()
        .filter_map(|line| line.strip_prefix(key)?.strip_prefix('='));
    let first = values
        .next()
        .ok_or_else(|| PortError::Parse(format!("systemctl show printed no {key}")))?;
    match (first.is_empty(), values.next()) {
        (_, Some(_)) => Err(PortError::Parse(format!(
            "systemctl show printed {key} twice"
        ))),
        (true, None) => Err(PortError::Parse(format!(
            "systemctl show printed an empty {key}"
        ))),
        (false, None) => Ok(first.to_owned()),
    }
}

impl Units for SystemUnits<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        let text = self
            .runner
            .run(
                SYSTEMCTL,
                &["show", "-p", "ActiveState,SubState", "--", unit],
            )
            .map_err(PortError::Command)?;
        Ok(UnitState {
            active: property(&text, "ActiveState")?,
            sub: property(&text, "SubState")?,
        })
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.runner
            .run(SYSTEMCTL, &["stop", "--no-block", "--", unit])
            .map(drop)
            .map_err(PortError::Command)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.runner
            .run(SYSTEMCTL, &["start", "--", unit])
            .map(drop)
            .map_err(PortError::Command)
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        Ok(self
            .runner
            .run(SYSTEMCTL, &["is-active", "--", unit])
            .is_ok_and(|text| text.trim() == "active"))
    }
}

/// The release link: a symbolic link to the directory of the release that runs.
pub trait Link {
    /// Where `link` points; `None` when there is no such link.
    ///
    /// # Errors
    /// `link` exists and is not a readable symbolic link.
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError>;

    /// Points `link` at `target`, atomically: a reader sees the old target or the new one and
    /// never neither. Does nothing when the link already points there.
    ///
    /// # Errors
    /// The link could not be made.
    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError>;
}

/// The name of the new link before it is renamed over the old: `<link>.relink`.
fn relink_name(link: &Path) -> PathBuf {
    let mut name = link.as_os_str().to_owned();
    name.push(".relink");
    PathBuf::from(name)
}

/// Reads the target of `link`; `None` when there is nothing at the path.
///
/// # Errors
/// The path exists and is not a symbolic link.
pub fn read_link(link: &Path) -> io::Result<Option<PathBuf>> {
    match fs::read_link(link) {
        Ok(target) => Ok(Some(target)),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(why) => Err(why),
    }
}

/// Points `link` at `target`: a new link beside it, renamed over it, and the directory synced.
/// A link that already points at `target` is left alone.
///
/// # Errors
/// Any step failed; the link is then what it was.
pub fn swap_link(link: &Path, target: &Path) -> io::Result<()> {
    if read_link(link)?.is_some_and(|now| now == target) {
        return Ok(());
    }
    let new = relink_name(link);
    // A leftover of an interrupted swap.
    match fs::remove_file(&new) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => return Err(why),
        _ => {}
    }
    symlink(target, &new)?;
    fs::rename(&new, link)?;
    // A bare name has the empty path for a parent: its directory is the current one.
    let dir = link
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::File::open(dir)?.sync_all()
}

/// [`Link`] on the machine's file system.
pub struct SystemLink;

impl Link for SystemLink {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        Ok(read_link(link)?)
    }

    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        Ok(swap_link(link, target)?)
    }
}

/// When a cordon lapses, on the lease server's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CordonView {
    /// Milliseconds since the epoch.
    pub expires_at_ms: u64,
}

/// What `GET /v1/hosts/{id}` says of the hook's own host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReading {
    /// The host id.
    pub id: String,
    /// `awaiting`, `live` or `lost`.
    pub state: String,
    /// One more at each hello of the host; 0 before the first since the server started.
    pub epoch: u64,
    /// When the current session began, on the server's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_started_ms: Option<u64>,
    /// The leases the host holds: 0 or 1.
    pub leases: u64,
    /// The lease, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// The host holds no lease but has runs to finish.
    pub releasing: bool,
    /// The cordon in force, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cordon: Option<CordonView>,
    /// The taint the host reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<Taint>,
    /// The `agent_version` of the last hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// The `runtime` of the last hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

/// The reply to a cordon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cordoned {
    /// The host was cordoned before this call.
    pub already_cordoned: bool,
    /// The host after the call.
    pub host: HostReading,
}

/// The reply to an uncordon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uncordoned {
    /// A cordon was in force and this call ended it.
    pub lifted: bool,
    /// The host after the call.
    pub host: HostReading,
}

/// Why the lease server did not answer as asked.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LeaseError {
    /// The server holds no host by this id (`unknown-host`): it has not said hello since the
    /// server started.
    #[error("unknown host")]
    UnknownHost,
    /// The server refused the request (`invalid-cordon`, ...), by its code.
    #[error("refused: {0}")]
    Refused(String),
    /// The server could not be reached, or answered what is not understood.
    #[error("{0}")]
    Unreachable(String),
}

/// The lease server's routes for the hook's own host.
pub trait Lease {
    /// `POST /v1/hosts/{id}:cordon {"lease_secs"}`: cordons for `lease_secs` from now (cut to
    /// [`MAX_CORDON_SECS`]). A repeat extends the cordon and never shortens it.
    ///
    /// # Errors
    /// [`LeaseError`].
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError>;

    /// `POST /v1/hosts/{id}:uncordon`. Succeeds when there was no cordon.
    ///
    /// # Errors
    /// [`LeaseError`].
    fn uncordon(&self) -> Result<Uncordoned, LeaseError>;

    /// `GET /v1/hosts/{id}`.
    ///
    /// # Errors
    /// [`LeaseError`].
    fn host(&self) -> Result<HostReading, LeaseError>;
}

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

/// Time, and waiting for it to pass.
pub trait Clock {
    /// Milliseconds since the epoch.
    fn now_ms(&self) -> u64;

    /// Waits `span`. The fake counts it and moves its time on, and waits for nothing.
    fn pause(&self, span: Duration);
}

/// [`Clock`] on the machine.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    fn pause(&self, span: Duration) {
        std::thread::sleep(span);
    }
}

/// A kill point: called before each change a step or a fake makes, so that a test binary can
/// stop the step there.
pub trait Gate {
    /// Called before a change.
    fn point(&self);
}

/// [`Gate`] on the machine: nothing.
pub struct NoGate;

impl Gate for NoGate {
    fn point(&self) {}
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
