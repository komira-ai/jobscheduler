//! The hook on this machine: the steps over the real unit manager ([`PlatformUnits`]), release
//! link, lease server and agent state.
//!
//! What a step needs of the machine comes from three places: the declaration, read from
//! [`DECLARATION_PATH`](super::DECLARATION_PATH); the params, whose [`ConnectParams`] say how to
//! reach the lease server and the agent's state; and the machine itself (its id, and how the
//! state directory's owner is looked at). A [`Machine`] holds the three seams, so a test can
//! run the same steps against a real lease server in its own process and a state directory it
//! owns.
//!
//! Nothing here is an environment variable. A lease that could not be made (no server in the
//! params, no credential, an id that is no id) is not an error of the hook as a whole: a step
//! that never asks the lease server (a first install) does not need it, and one that does ends
//! with `lease_unreachable`, class environment, saying why.
//!
//! The agent's state root is looked at before any step runs. An update needs the state root of
//! the agent that runs, so one the params do not name, or name and is not there, ends every
//! step `state_invalid`: an absent root would otherwise read as "no taint, nothing owed". A
//! state root that is there and is not private (a link, a file, another owner, open to others)
//! ends every step `state_invalid` too, a first install's included. Only a first install may
//! have none. Either error starts with the param's name, `state_root`.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::Taint;
use js_mac_provision::daemon::AGENT_EXIT_TIMEOUT_SECS;
use js_types::HostId;

use crate::CredentialFile;
use crate::facts::{
    CommandRunner, FileReader, Platform, SystemCommands, SystemFiles, host_id, machine_id,
};
use crate::lifecycle::StateRoot;
use crate::own_host::{Cordoned, HostReading, HttpLease, Lease, LeaseError, Uncordoned};

use super::declaration::{DECLARATION_PATH, Declaration};
use super::ports::{NoGate, Ports, StateView, SystemClock, SystemLink};
use super::step::{self, Env};
use super::{
    ConnectParams, DirStat, Done, HOST_ID_KEY, Invocation, LaunchUnits, Outcome, Params,
    PlatformUnits, PortError, Reason, Report, SERVER_KEY, STATE_ROOT_KEY, Step, StopRunner,
    fd_stat,
};

/// How a directory of the state directory's path is looked at (see [`fd_stat`]).
pub type StatFn = dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>;

/// What the hook takes from the machine besides the params.
pub struct Machine<'a> {
    /// Where the declaration is.
    pub declaration: &'a Path,
    /// How the state directory and the directories above it are looked at.
    pub stat: &'a StatFn,
    /// The id this machine registers under, when the params name none.
    pub own_host_id: &'a dyn Fn() -> Result<String, String>,
}

fn system_stat(_: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
    fd_stat(fd)
}

/// The id the agent registers under, a salted hash of the machine id that `runner` and `files`
/// read on `platform`.
///
/// # Errors
/// The machine id could not be read.
pub fn own_host_id(
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<String, String> {
    machine_id(platform, runner, files).map(|machine| host_id(&machine))
}

fn system_host_id() -> Result<String, String> {
    own_host_id(
        &Platform::current(),
        &SystemCommands::default(),
        &SystemFiles,
    )
}

impl Machine<'static> {
    /// This machine: the declaration the updater installs, directories looked at as they are,
    /// and the agent's own host id.
    #[must_use]
    pub fn system() -> Self {
        Self {
            declaration: Path::new(DECLARATION_PATH),
            stat: &system_stat,
            own_host_id: &system_host_id,
        }
    }
}

/// The file the bearer is read from: the host credential of the state root when it holds one,
/// which is what the agent sends once it has one, and otherwise the token file. A state root
/// that is there and not private is an error of its own: it is not trusted with a credential,
/// and the join token is not used in its place (the server would refuse it on the host's own
/// routes, which says nothing of the cause).
fn credential(params: &ConnectParams) -> Result<PathBuf, String> {
    let kept = match find_root(params.state_root.as_deref())? {
        Root::Open(root) => {
            let file = CredentialFile::in_dir(root.path());
            file.load()?.map(|_| file.path())
        }
        Root::Absent => None,
    };
    kept.or_else(|| params.token_file.clone())
        .ok_or_else(|| "token_file: required unless state_root holds a host credential".to_owned())
}

/// The lease server's own-host routes for the host the params name; a connection that could not
/// be made answers every call with the reason it could not.
pub struct MachineLease {
    client: Result<HttpLease, String>,
}

impl MachineLease {
    /// Connects as the params say; `own_host_id` is asked when they name no host.
    #[must_use]
    pub fn connect(
        params: &ConnectParams,
        own_host_id: &dyn Fn() -> Result<String, String>,
    ) -> Self {
        Self {
            client: Self::open(params, own_host_id),
        }
    }

    fn open(
        params: &ConnectParams,
        own_host_id: &dyn Fn() -> Result<String, String>,
    ) -> Result<HttpLease, String> {
        let server = params
            .server
            .as_deref()
            .ok_or_else(|| format!("the hook params name no lease server ({SERVER_KEY})"))?;
        let id = match &params.host_id {
            Some(id) => id.clone(),
            None => own_host_id()?,
        };
        HostId::new(&id).map_err(|why| format!("{HOST_ID_KEY}: {why}"))?;
        let credential = credential(params)?;
        HttpLease::connect(server, &credential, params.ca_cert.as_deref(), &id)
            .map_err(|why| format!("the lease client: {why}"))
    }

    fn client(&self) -> Result<&HttpLease, LeaseError> {
        self.client
            .as_ref()
            .map_err(|why| LeaseError::Unreachable(why.clone()))
    }
}

impl Lease for MachineLease {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        self.client()?.cordon(lease_secs)
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        self.client()?.uncordon()
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        self.client()?.host()
    }
}

/// The agent's state root as the hook finds it.
enum Root {
    /// None is named, or none is there (the agent never ran): no taint, no owed check. Only a
    /// first install may find this (see [`agent_state`]).
    Absent,
    /// A private directory of the agent's.
    Open(StateRoot),
}

/// The state root at `path`: [`Root::Absent`] when `path` is `None` or nothing is at it. One
/// that is there and is not usable (a link, a dangling one included, a file, another owner,
/// open to others) or that cannot be looked at is an error, whose text starts with
/// [`STATE_ROOT_KEY`]: nothing can be said of a taint in it.
fn find_root(path: Option<&Path>) -> Result<Root, String> {
    let Some(path) = path else {
        return Ok(Root::Absent);
    };
    // The path itself, not what a link at it points to: a link is refused by the open.
    match std::fs::symlink_metadata(path) {
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(Root::Absent),
        _ => StateRoot::open_named(path, STATE_ROOT_KEY).map(Root::Open),
    }
}

/// The state root the step reads: an update must find the private state root of the agent that
/// runs; a first install may find none.
///
/// # Errors
/// The state root is there and not usable, or it is an update's and the params name none or
/// name one that is not there. The text starts with [`STATE_ROOT_KEY`].
fn agent_state(params: &Params) -> Result<Root, String> {
    let named = params.connect.state_root.as_deref();
    let root = find_root(named)?;
    if !matches!(root, Root::Absent) || params.previous_version.is_none() {
        return Ok(root);
    }
    Err(match named {
        None => format!(
            "{STATE_ROOT_KEY}: an update needs the state root of the agent that runs, and the \
             params name none"
        ),
        Some(path) => format!(
            "{STATE_ROOT_KEY}: {} is not there, and an update needs the state root of the agent \
             that runs",
            path.display()
        ),
    })
}

impl StateView for Root {
    fn taint(&self) -> Option<Taint> {
        match self {
            Root::Open(root) => StateView::taint(root),
            Root::Absent => None,
        }
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        match self {
            Root::Open(root) => StateView::owed(root),
            Root::Absent => Ok(Vec::new()),
        }
    }
}

/// The unit manager of this platform: on a Mac, [`launch_units`].
#[cfg(target_os = "macos")]
fn platform_units<'a>(commands: &'a SystemCommands, stop: &'a StopRunner) -> PlatformUnits<'a> {
    launch_units(commands, stop)
}

#[cfg(not(target_os = "macos"))]
fn platform_units<'a>(commands: &'a SystemCommands, _stop: &'a StopRunner) -> PlatformUnits<'a> {
    super::SystemUnits::new(commands)
}

/// The units of a Mac: `launchctl` through `commands`, `bootout` through `stop` (it waits for the
/// agent to exit, which its plist allows [`AGENT_EXIT_TIMEOUT_SECS`], so `stop` must wait
/// longer: see [`StopRunner`]), and a kickstart held back while the agent's token file
/// ([`GATE`](js_mac_provision::daemon::GATE)) is absent. Built on every platform, so that a test
/// off a Mac checks the wiring the Mac's hook runs with.
#[must_use]
pub fn launch_units<'a>(
    commands: &'a dyn CommandRunner,
    stop: &'a dyn CommandRunner,
) -> LaunchUnits<'a> {
    LaunchUnits::new(commands)
        .with_stop_runner(stop)
        .with_gate(Path::new(js_mac_provision::daemon::GATE))
}

/// Runs the step the invocation names on this machine.
#[must_use]
pub fn run_step(invocation: &Invocation, params: &Params, machine: &Machine<'_>) -> Done {
    let declaration = match Declaration::read(machine.declaration) {
        Ok(declaration) => declaration,
        Err(why) => {
            return Done {
                outcome: Outcome::Failure,
                report: Report::failure(
                    Reason::StateInvalid,
                    format!("{}: {why}", machine.declaration.display()),
                ),
            };
        }
    };
    let state = match agent_state(params) {
        Ok(state) => state,
        Err(why) => {
            return Done {
                outcome: Outcome::Failure,
                report: Report::failure(Reason::StateInvalid, why),
            };
        }
    };
    let commands = SystemCommands::default();
    let stop = StopRunner::new(Duration::from_secs(u64::from(AGENT_EXIT_TIMEOUT_SECS)));
    let units = platform_units(&commands, &stop);
    let lease = MachineLease::connect(&params.connect, machine.own_host_id);
    let env = Env {
        ports: Ports {
            units: &units,
            link: &SystemLink,
            lease: &lease,
            state: &state,
            clock: &SystemClock,
            gate: &NoGate,
        },
        runner: &commands,
        declaration: &declaration,
        stat: machine.stat,
    };
    match invocation.step {
        Step::Prepare => step::prepare(&env, params),
        Step::Cutover => step::cutover(&env, params),
        Step::Verify => step::verify(&env, params),
        Step::Rollback => step::rollback(&env, params),
        Step::Cleanup => step::cleanup(&env, params),
    }
}
