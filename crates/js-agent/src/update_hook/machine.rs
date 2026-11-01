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

use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::Taint;
use js_mac_provision::daemon::AGENT_EXIT_TIMEOUT_SECS;
use js_types::HostId;

use crate::CredentialFile;
use crate::facts::{Platform, SystemCommands, SystemFiles, host_id, machine_id};
use crate::lifecycle::StateRoot;
use crate::own_host::{Cordoned, HostReading, HttpLease, Lease, LeaseError, Uncordoned};

use super::declaration::{DECLARATION_PATH, Declaration};
use super::ports::{NoGate, Ports, StateView, SystemClock, SystemLink};
use super::step::{self, Env};
use super::{
    ConnectParams, DirStat, Done, HOST_ID_KEY, Invocation, Outcome, Params, PlatformUnits,
    PortError, Reason, Report, SERVER_KEY, Step, StopRunner, fd_stat,
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

/// The id the agent registers under: a salted hash of the machine id.
fn system_host_id() -> Result<String, String> {
    machine_id(
        &Platform::current(),
        &SystemCommands::default(),
        &SystemFiles,
    )
    .map(|machine| host_id(&machine))
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
/// that is not private is not trusted with a credential.
fn credential(params: &ConnectParams) -> Result<PathBuf, String> {
    let kept = match Root::find(params.state_root.as_deref()) {
        Root::Open(root) => {
            let file = CredentialFile::in_dir(root.path());
            file.load()?.map(|_| file.path())
        }
        Root::Absent | Root::Unusable(_) => None,
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
    /// None is named, or none is there (the agent never ran): no taint, no owed check.
    Absent,
    /// A private directory of the agent's.
    Open(StateRoot),
    /// There is one and it is not usable (a link, another owner, open to others): the text
    /// says why. Nothing can be said of a taint in it, so a step that reads it fails.
    Unusable(String),
}

impl Root {
    fn find(path: Option<&Path>) -> Root {
        let Some(path) = path else {
            return Root::Absent;
        };
        match path.try_exists() {
            Ok(false) => Root::Absent,
            _ => match StateRoot::open(path) {
                Ok(root) => Root::Open(root),
                Err(text) => Root::Unusable(text),
            },
        }
    }
}

impl StateView for Root {
    fn taint(&self) -> Option<Taint> {
        match self {
            Root::Open(root) => StateView::taint(root),
            Root::Absent | Root::Unusable(_) => None,
        }
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        match self {
            Root::Open(root) => StateView::owed(root),
            Root::Absent => Ok(Vec::new()),
            Root::Unusable(text) => Err(PortError::Io(text.clone())),
        }
    }
}

/// The unit manager of this platform. A Mac's `bootout` waits for the agent to exit, which its
/// plist allows [`AGENT_EXIT_TIMEOUT_SECS`]: the stop is run by a runner that waits longer.
#[cfg(target_os = "macos")]
fn platform_units<'a>(commands: &'a SystemCommands, stop: &'a StopRunner) -> PlatformUnits<'a> {
    super::LaunchUnits::new(commands)
        .with_stop_runner(stop)
        .with_gate(Path::new(js_mac_provision::daemon::GATE))
}

#[cfg(not(target_os = "macos"))]
fn platform_units<'a>(commands: &'a SystemCommands, _stop: &'a StopRunner) -> PlatformUnits<'a> {
    super::SystemUnits::new(commands)
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
    let commands = SystemCommands::default();
    let stop = StopRunner::new(Duration::from_secs(u64::from(AGENT_EXIT_TIMEOUT_SECS)));
    let units = platform_units(&commands, &stop);
    let lease = MachineLease::connect(&params.connect, machine.own_host_id);
    let state = Root::find(params.connect.state_root.as_deref());
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
        // `rollback` has no body yet.
        Step::Rollback => Done {
            outcome: Outcome::Failure,
            report: Report::failure(Reason::NotImplemented, "rollback is not implemented"),
        },
        Step::Cleanup => step::cleanup(&env, params),
    }
}
