//! The hook on this machine: the steps over the real unit manager ([`PlatformUnits`]), release
//! link, lease server and agent state.
//!
//! What a step needs of the machine comes from three places: the params, which name the agent's
//! state root and token file; the host's flags file (`flags_file`, the file `js-agent
//! --flags-file` reads), which names the lease server, the certificate to trust and the host id
//! ([`ConnectParams`]); and the machine itself (its id, how the state directory's owner and the
//! token file are looked at). A [`Machine`] holds those seams, so a test can run the same steps
//! against a real lease server in its own process and a state directory it owns.
//!
//! Nothing here is an environment variable. A lease that could not be made (a flags file that
//! cannot be used, no credential, an id that is no id) is not an error of the hook as a whole: a
//! step that never asks the lease server (a first install) does not need it, and one that does
//! ends with `lease_unreachable`, class environment, saying why.
//!
//! The agent's state root is looked at before any step runs. An update needs the state root of
//! the agent that runs, so one that is not there ends every step `state_invalid`: an absent root
//! would otherwise read as "no taint, nothing owed". A state root that is there and is not
//! private (a link, a file, another owner, open to others) ends every step `state_invalid` too,
//! a first install's included, and the join token is never used in its place. Only a first
//! install may find none. Either error starts with the param's name, `state_root`. The check
//! comes before the step's lock and changes nothing; a `cutover` or `rollback` whose record says
//! it is past its commit still says `reversible: false` (the record is read without the lock).
//! The state root is looked up once per step: the lease's credential and the step's taint and
//! owed checks come from the same [`StateRoot`].

use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use js_agent_wire::Taint;
use js_client::ClientError;
use js_mac_provision::daemon::AGENT_EXIT_TIMEOUT_SECS;
use js_types::HostId;
use js_update_hook::peek;

use crate::CredentialFile;
use crate::cli::Cli;
use crate::facts::{
    CommandRunner, FileReader, Platform, SystemCommands, SystemFiles, host_id, machine_id,
};
use crate::flags_file::{self, FileFacts};
use crate::lifecycle::StateRoot;
use crate::own_host::{
    Cordoned, HostReading, HttpLease, Lease, LeaseError, RequestLimit, Uncordoned,
};

use super::ports::{NoGate, Ports, StateView, SystemClock, SystemLink};
use super::step::{self, Env, TokenFacts, TokenFn};
use super::{
    ConnectParams, DirStat, Done, FLAGS_FILE_KEY, Invocation, Manager, Outcome, Params,
    PlatformUnits, PortError, Reason, Report, STATE_ROOT_KEY, Step, StopRunner, TOKEN_FILE_KEY,
    fd_stat,
};

/// How a directory of the state directory's path is looked at (see [`fd_stat`]).
pub type StatFn = dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>;

/// How the host's flags file is read (see [`flags_file::read_system`]).
pub type FlagsFn = dyn Fn(&Path) -> io::Result<FileFacts>;

/// What the hook takes from the machine besides the params.
pub struct Machine<'a> {
    /// How the state directory and the directories above it are looked at.
    pub stat: &'a StatFn,
    /// The id this machine registers under, when the flags file names none.
    pub own_host_id: &'a dyn Fn() -> Result<String, String>,
    /// How the join token file is looked at.
    pub token: &'a TokenFn,
    /// How the flags file is read.
    pub flags: &'a FlagsFn,
    /// The user the hook runs as: who may own the flags file besides root, and who must own
    /// the token file.
    pub euid: u32,
}

fn system_stat(_: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
    fd_stat(fd)
}

/// The token file as the system has it, a link followed (secret mounts are links).
fn system_token(path: &Path) -> io::Result<TokenFacts> {
    let meta = std::fs::metadata(path)?;
    Ok(TokenFacts {
        regular: meta.is_file(),
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
    })
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
    /// This machine: directories and files looked at as they are, and the agent's own host id.
    #[must_use]
    pub fn system() -> Self {
        Self {
            stat: &system_stat,
            own_host_id: &system_host_id,
            token: &system_token,
            flags: &flags_file::read_system,
            euid: rustix::process::geteuid().as_raw(),
        }
    }
}

/// The file the bearer is read from, with the params key that led to it: the host credential
/// of the state root when it holds one, which is what the agent sends once it has one, and
/// otherwise the token file. A state root that is there and not private is an error of its own:
/// it is not trusted with a credential, and the join token is not used in its place (the server
/// would refuse it on the host's own routes, which says nothing of the cause). A credential file
/// that is there and cannot be read, or holds no credential, is refused by the key
/// `state_root`, and the token file is not used in its place either: the agent would not use it.
/// `root` is what the hook found at `state_root`.
fn credential(
    params: &ConnectParams,
    root: Result<&Root, &RootError>,
) -> Result<(PathBuf, &'static str), String> {
    let kept = match root.map_err(RootError::to_string)? {
        Root::Open(root) => {
            let file = CredentialFile::in_dir(root.path());
            let loaded = file
                .load()
                .map_err(|why| format!("{STATE_ROOT_KEY}: {why}"))?;
            loaded.map(|_| (file.path(), STATE_ROOT_KEY))
        }
        Root::Absent => None,
    };
    kept.or_else(|| params.token_file.clone().map(|file| (file, TOKEN_FILE_KEY)))
        .ok_or_else(|| {
            format!("{TOKEN_FILE_KEY}: required unless {STATE_ROOT_KEY} holds a host credential")
        })
}

/// Why the client could not be built, as the client says it. The client's text begins with the
/// command-line flag of the programs it serves (`--server: `, `--token-file: `, `--ca-cert: `):
/// the server and the certificate are flags of the host's flags file, so their text is told
/// as it is; the token flag is not one the hook has, only its params, and the credential may
/// be the state root's rather than a token file's (`credential_key` says which).
fn named(error: &ClientError, credential_key: &str) -> String {
    let text = error.to_string();
    match text.strip_prefix("--token-file: ") {
        Some(why) => format!("{credential_key}: {why}"),
        None => text,
    }
}

/// The lease server's own-host routes for the host the params name; a connection that could not
/// be made answers every call with the reason it could not.
pub struct MachineLease {
    client: Result<HttpLease, String>,
}

impl MachineLease {
    /// Connects as the params say; `own_host_id` is asked when they name no host. Every call
    /// ends within `limit` (the machine's is [`RequestLimit::DEFAULT`]): a server that is down
    /// or does not answer is [`LeaseError::Unreachable`], and the steps that wait on the server
    /// bound their waits by their own clock.
    #[must_use]
    pub fn connect(
        params: &ConnectParams,
        own_host_id: &dyn Fn() -> Result<String, String>,
        limit: RequestLimit,
    ) -> Self {
        let root = find_root(params.state_root.as_deref());
        Self::with_root(params, root.as_ref(), own_host_id, limit)
    }

    /// [`MachineLease::connect`] with the state root already looked up: `run_step` gives the
    /// one its checks found, so the credential is read from the state root the step reads.
    fn with_root(
        params: &ConnectParams,
        root: Result<&Root, &RootError>,
        own_host_id: &dyn Fn() -> Result<String, String>,
        limit: RequestLimit,
    ) -> Self {
        Self {
            client: Self::open(params, root, own_host_id, limit),
        }
    }

    fn open(
        params: &ConnectParams,
        root: Result<&Root, &RootError>,
        own_host_id: &dyn Fn() -> Result<String, String>,
        limit: RequestLimit,
    ) -> Result<HttpLease, String> {
        let server = params
            .server
            .as_deref()
            .ok_or_else(|| "no lease server is named (--server)".to_owned())?;
        let id = match &params.host_id {
            Some(id) => id.clone(),
            None => own_host_id()?,
        };
        HostId::new(&id).map_err(|why| format!("--host-id: {why}"))?;
        let (credential, key) = credential(params, root)?;
        HttpLease::connect(server, &credential, params.ca_cert.as_deref(), &id, limit)
            .map_err(|why| named(&why, key))
    }

    /// A connection that could not be made, for the reason `why`.
    fn failed(why: String) -> Self {
        Self { client: Err(why) }
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

/// How to reach the lease server: the server, the certificate and the host id of the host's own
/// agent flags (`flags_file`), read as `js-agent --flags-file` reads them, with the state root
/// and the token file of the params.
///
/// # Errors
/// The flags file cannot be used (see [`flags_file::expand_with`]), or its flags are not the
/// agent's (the server and the pool are required, as for the agent). The text starts with the
/// params key, `flags_file`.
fn connect_of(params: &Params, machine: &Machine<'_>) -> Result<ConnectParams, String> {
    let args = vec![
        "js-agent".into(),
        flags_file::FLAG.into(),
        params.flags_file.clone().into_os_string(),
    ];
    let words = flags_file::expand_with(args, machine.euid, machine.flags)
        .map_err(|why| format!("{FLAGS_FILE_KEY}: {why}"))?;
    let cli = Cli::try_parse_from(words).map_err(|why| {
        let text = why.to_string();
        let reason: Vec<&str> = text
            .lines()
            .take_while(|line| !line.starts_with("Usage:"))
            .filter(|line| !line.trim().is_empty())
            .collect();
        format!("{FLAGS_FILE_KEY}: {}", reason.join(" "))
    })?;
    Ok(ConnectParams {
        server: Some(cli.server),
        state_root: Some(params.state_root.clone()),
        token_file: Some(params.token_file.clone()),
        ca_cert: cli.ca_cert,
        host_id: cli.host_id,
    })
}

/// The agent's state root as the hook finds it.
enum Root {
    /// None is named, or none is there (the agent never ran): no taint, no owed check. Only a
    /// first install may find this (see [`agent_state`]).
    Absent,
    /// A private directory of the agent's.
    Open(StateRoot),
}

/// Why the hook will not read the agent's state root: every step ends `state_invalid` with it,
/// before it takes its lock. The text starts with the params key, [`STATE_ROOT_KEY`].
#[derive(Debug, thiserror::Error)]
enum RootError {
    /// An update, and nothing is at the path the params name.
    #[error(
        "{key}: {} is not there, and an update needs the state root of the agent that runs",
        .0.display(),
        key = STATE_ROOT_KEY
    )]
    NotThere(PathBuf),
    /// Something is at `path`, and it is not a private directory of the user the hook runs
    /// as (a link, a dangling one included, a file, another owner, open to others), or it
    /// cannot be looked at. `why` is the agent's own check's, as [`StateRoot::open`] says it.
    #[error("{key}: {}: {why}", path.display(), key = STATE_ROOT_KEY)]
    Unusable {
        /// The path the params name.
        path: PathBuf,
        /// Why it is not a state root.
        why: String,
    },
}

/// The prefix of [`StateRoot::open`]'s errors: the agent's flag, which the hook does not have.
const AGENT_FLAG: &str = "--state-root: ";

/// The state root at `path`: [`Root::Absent`] when `path` is `None` or nothing is at it.
///
/// # Errors
/// [`RootError::Unusable`]: something is at `path` and it is not usable, or the path cannot be
/// looked at. Nothing can be said of a taint in it.
fn find_root(path: Option<&Path>) -> Result<Root, RootError> {
    let Some(path) = path else {
        return Ok(Root::Absent);
    };
    // The path itself, not what a link at it points to: a link is refused by the open.
    match std::fs::symlink_metadata(path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(Root::Absent),
        _ => StateRoot::open(path)
            .map(Root::Open)
            .map_err(|text| RootError::Unusable {
                path: path.to_owned(),
                why: text.strip_prefix(AGENT_FLAG).unwrap_or(&text).to_owned(),
            }),
    }
}

/// The state root the step reads: an update must find the private state root of the agent that
/// runs; a first install may find none.
///
/// # Errors
/// What [`find_root`] says; for an update, [`RootError::NotThere`] when it finds none.
fn agent_state(params: &Params) -> Result<Root, RootError> {
    let root = find_root(Some(&params.state_root))?;
    if !matches!(root, Root::Absent) || params.previous_version.is_none() {
        return Ok(root);
    }
    Err(RootError::NotThere(params.state_root.clone()))
}

/// The ending of a step whose state root the hook will not read: `state_invalid` with `why`,
/// before the lock is taken and with nothing changed. A `cutover` or `rollback` whose record
/// says it is past its commit ([`step::committed`]) says `reversible: false`, as its body would;
/// the record is read without the lock ([`peek`]). A record that cannot be read, or that the
/// rules of the params refuse ([`validate`](super::validate)), says nothing of a commit, as for
/// a step whose body cannot read or refuses its record under the lock.
fn refused(step: Step, params: &Params, stat: &StatFn, why: &RootError) -> Done {
    let committed = peek(&params.settings.state_dir, stat)
        .ok()
        .flatten()
        .filter(|record| super::validate(record, params).is_ok())
        .is_some_and(|record| step::committed(step, &record));
    Done {
        outcome: Outcome::Failure,
        report: Report {
            reversible: !committed,
            ..Report::failure(Reason::StateInvalid, why.to_string())
        },
    }
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

    fn has_credential(&self) -> bool {
        match self {
            Root::Open(root) => StateView::has_credential(root),
            Root::Absent => false,
        }
    }
}

/// The unit manager of this platform: on a Mac, [`launch_units`], gated on `token_file`.
#[cfg(target_os = "macos")]
fn platform_units<'a>(
    commands: &'a SystemCommands,
    stop: &'a StopRunner,
    token_file: &'a Path,
) -> PlatformUnits<'a> {
    launch_units(commands, stop, token_file)
}

#[cfg(not(target_os = "macos"))]
fn platform_units<'a>(
    commands: &'a SystemCommands,
    _stop: &'a StopRunner,
    _token_file: &'a Path,
) -> PlatformUnits<'a> {
    super::SystemUnits::new(commands)
}

/// The unit manager the hook is built for.
const MANAGER: Manager = if cfg!(target_os = "macos") {
    Manager::Launchd
} else {
    Manager::Systemd
};

/// The units of a Mac: `launchctl` through `commands`, `bootout` through `stop` (it waits for the
/// agent to exit, which its plist allows [`AGENT_EXIT_TIMEOUT_SECS`], so `stop` must wait
/// longer: see [`StopRunner`]), and a kickstart held back while the agent's token file (the
/// `token_file` param, which the plist's `KeepAlive` also waits on) is absent. Built for the
/// tests on every platform, so that a test off a Mac checks the wiring the Mac's hook runs with.
#[cfg(any(target_os = "macos", test))]
fn launch_units<'a>(
    commands: &'a dyn CommandRunner,
    stop: &'a dyn CommandRunner,
    token_file: &'a Path,
) -> super::LaunchUnits<'a> {
    super::LaunchUnits::new(commands)
        .with_stop_runner(stop)
        .with_gate(token_file)
}

/// Runs the step the invocation names on this machine.
#[must_use]
pub fn run_step(invocation: &Invocation, params: &Params, machine: &Machine<'_>) -> Done {
    let state = match agent_state(params) {
        Ok(state) => state,
        Err(why) => return refused(invocation.step, params, machine.stat, &why),
    };
    let commands = SystemCommands::default();
    let stop = StopRunner::new(Duration::from_secs(u64::from(AGENT_EXIT_TIMEOUT_SECS)));
    let units = platform_units(&commands, &stop, &params.token_file);
    let lease = match connect_of(params, machine) {
        Ok(connect) => MachineLease::with_root(
            &connect,
            Ok(&state),
            machine.own_host_id,
            RequestLimit::DEFAULT,
        ),
        Err(why) => MachineLease::failed(why),
    };
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
        manager: MANAGER,
        euid: machine.euid,
        token: machine.token,
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use js_client::ClientError;

    use super::{CommandRunner, launch_units, named};
    use crate::update_hook::{LAUNCHCTL, Units};

    /// A `launchctl` that shows every service running, and notes each command it is given.
    #[derive(Default)]
    struct Noting {
        seen: RefCell<Vec<String>>,
    }

    impl CommandRunner for Noting {
        fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
            self.seen
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok("\tstate = running\n".to_owned())
        }

        fn run_in(
            &self,
            program: &str,
            args: &[&str],
            _: &[(&str, &str)],
        ) -> Result<String, String> {
            self.run(program, args)
        }
    }

    /// Asserts the wiring of a Mac's units: a read, an `is_active` and the start of a running
    /// service each print the service through the commands; a stop prints it through the
    /// commands and boots it out through the stop runner, which runs nothing else; and the
    /// kickstart waits on the agent's token file. It catches units built without the stop
    /// runner or the gate, or with the two runners swapped or one runner for both.
    #[test]
    fn the_units_of_a_mac_read_through_the_commands_stop_through_the_stop_runner_and_wait_on_the_token()
     {
        let label = "com.example.agent";
        let token = Path::new("/etc/example/token");
        let print = format!("{LAUNCHCTL} print system/{label}");
        let commands = Noting::default();
        let stop = Noting::default();
        let units = launch_units(&commands, &stop, token);
        assert_eq!(units.show(label).unwrap().active, "active");
        assert!(units.is_active(label).unwrap());
        units.start(label).unwrap();
        assert!(stop.seen.borrow().is_empty());
        units.stop_no_block(label).unwrap();
        assert_eq!(
            *stop.seen.borrow(),
            [format!("{LAUNCHCTL} bootout system/{label}")]
        );
        assert_eq!(*commands.seen.borrow(), vec![print; 4]);
        assert_eq!(units.gate(), Some(token));
        // The fake's other entry, which the units never use, answers as `run` does.
        let other = Noting::default();
        assert_eq!(
            other.run_in(LAUNCHCTL, &["list"], &[]),
            other.run(LAUNCHCTL, &["list"])
        );
    }

    /// Asserts that a client error the hook has no params key for keeps the client's own
    /// text: an error that names no flag (today only a request gives one, never the building
    /// of a client), and a flag other than the three the hook maps. It catches a mapping that
    /// would blame the token file, or put any params key, in front of a text that is not about
    /// it. The tests of the mapped keys are in `tests/agent/update_hook/lease_port.rs`.
    #[test]
    fn a_client_error_the_hook_has_no_key_for_is_told_as_it_is() {
        let error = ClientError::Server("cannot talk to http://127.0.0.1:1".to_owned());
        assert_eq!(
            named(&error, "token_file"),
            "cannot talk to http://127.0.0.1:1"
        );
        let error = ClientError::Flag {
            flag: "--other",
            why: "is wrong".to_owned(),
        };
        assert_eq!(named(&error, "token_file"), "--other: is wrong");
    }
}
