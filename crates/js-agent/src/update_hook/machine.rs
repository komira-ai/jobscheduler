//! The hook on this machine: the steps over the real unit manager ([`PlatformUnits`]), release
//! link, lease server and agent state.
//!
//! What a step needs of the machine comes from three places: the params, which name the agent's
//! state root and token file; the host's flags file (`flags_file`, the file `js-agent
//! --flags-file` reads), which names the lease server, the certificate to trust, the host id and
//! the host's own certificate and key ([`ConnectParams`]); and the machine itself (its id, how the state directory's owner and the
//! token file are looked at). A [`Machine`] holds those seams, so a test can run the same steps
//! against a real lease server in its own process and a state directory it owns.
//!
//! Nothing here is an environment variable. A lease that could not be made (a flags file that
//! cannot be used, no credential, an id that is no id) is not an error of the hook as a whole: a
//! step that never asks the lease server (a first install) does not need it, and one that does
//! ends with `lease_unreachable`, class environment, saying why.
//! A first install reads the flags file only to see whether it names the host certificate and
//! key, which stand in for the join token; a flags file that cannot be used names none, and the
//! install goes by the token file as it always did.
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
//!
//! `drain` and `resume` ([`run_maintain`]) hold the state root to the same rule, and find the
//! hold file there: one that is not private ends either `state_invalid`. One that is not there
//! ends `drain` `state_invalid` (no agent runs to be drained) and `resume` with success and
//! nothing changed (no agent ever ran, so none was held). Their host facts are read on the
//! machine as the agent reads them when it starts ([`host_facts`]).

use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::Taint;
use js_client::{ClientError, HostIdentity};
use js_mac_provision::daemon::AGENT_EXIT_TIMEOUT_SECS;
use js_types::HostId;
use js_update_hook::peek;

use crate::cli::{Cli, parse_agent_flags, resolve_host_id};
use crate::facts::{
    CommandRunner, FileReader, Platform, SystemCommands, SystemFiles, detect, host_id, machine_id,
};
use crate::flags_file::{self, FileFacts};
use crate::lifecycle::StateRoot;
use crate::own_host::{
    Cordoned, HostReading, HttpLease, Lease, LeaseError, RequestLimit, Uncordoned,
};
use crate::{CredentialFile, Login};

use super::ports::{Facts, NoGate, Ports, StateView, SystemClock, SystemLink};
use super::step::{self, Env, FSTAB_LEFTOVER, LookFn, MaintainEnv, PathFacts, TokenFacts, TokenFn};
use super::{
    ConnectParams, DirStat, Done, DrainParams, FLAGS_FILE_KEY, Invocation, MaintainInvocation,
    MaintenanceStep, Manager, Outcome, Params, PlatformUnits, PortError, Reason, Report,
    STATE_ROOT_KEY, Step, StopRunner, TOKEN_FILE_KEY, fd_stat,
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
    /// How the host setup of a Mac looks at the state root and the scratch root.
    pub look: &'a LookFn,
    /// The copy of the file system table an earlier release's host setup of a Mac left beside
    /// it, which the host setup removes.
    pub fstab_leftover: &'a Path,
}

fn system_stat(_: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
    fd_stat(fd)
}

/// A root as the system has it, a link followed: its file system's device and its owner.
fn system_look(path: &Path) -> io::Result<PathFacts> {
    let meta = std::fs::metadata(path)?;
    Ok(PathFacts {
        device: meta.dev(),
        uid: meta.uid(),
    })
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
            look: &system_look,
            fstab_leftover: Path::new(FSTAB_LEFTOVER),
        }
    }
}

/// How the lease connection proves who it is, with the params key that led to it: the host's
/// certificate and key when the flags file names them (the agent presents them then), else the
/// host credential of the state root when it holds one, which is what the agent sends once it
/// has one, and otherwise the token file. A state root that is there and not private is an error
/// of its own: it is not trusted with a credential, and the join token is not used in its place
/// (the server would refuse it on the host's own routes, which says nothing of the cause). A
/// credential file that is there and cannot be read, or holds no credential, is refused by the
/// key `state_root`, and the token file is not used in its place either: the agent would not use
/// it. `root` is what the hook found at `state_root`; a host certificate needs none.
fn credential(
    params: &ConnectParams,
    root: Result<&Root, &RootError>,
) -> Result<(Login, &'static str), String> {
    if let Some(identity) = &params.host_identity {
        return Ok((Login::HostCertificate(identity.clone()), FLAGS_FILE_KEY));
    }
    let kept = match root.map_err(RootError::to_string)? {
        Root::Open(root) => {
            let file = CredentialFile::in_dir(root.path());
            let loaded = file
                .load()
                .map_err(|why| format!("{STATE_ROOT_KEY}: {why}"))?;
            loaded.map(|_| (Login::Token(file.path()), STATE_ROOT_KEY))
        }
        Root::Absent => None,
    };
    kept.or_else(|| {
        let file = params.token_file.clone();
        file.map(|file| (Login::Token(file), TOKEN_FILE_KEY))
    })
    .ok_or_else(|| {
        format!("{TOKEN_FILE_KEY}: required unless {STATE_ROOT_KEY} holds a host credential")
    })
}

/// Why the client could not be built, as the client says it. The client's text begins with the
/// command-line flag of the programs it serves (`--server: `, `--token-file: `, `--ca-cert: `,
/// `--host-cert: `, `--host-key: `): the server, the certificate to trust and the host
/// certificate and key are flags of the host's flags file, so their text is told as it is; the
/// token flag is not one the hook has, only its params, and the credential may be the state
/// root's rather than a token file's (`credential_key` says which).
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
    /// The bearer is the token file's and not a host credential of the state root: the
    /// server then refuses the own-host routes when the token is the pool's shared one.
    token_file: bool,
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
        let (client, token_file) = match Self::open(params, root, own_host_id, limit) {
            Ok((client, key)) => (Ok(client), key == TOKEN_FILE_KEY),
            Err(why) => (Err(why), false),
        };
        Self { client, token_file }
    }

    fn open(
        params: &ConnectParams,
        root: Result<&Root, &RootError>,
        own_host_id: &dyn Fn() -> Result<String, String>,
        limit: RequestLimit,
    ) -> Result<(HttpLease, &'static str), String> {
        let server = params
            .server
            .as_deref()
            .ok_or_else(|| "no lease server is named (--server)".to_owned())?;
        let id = match &params.host_id {
            Some(id) => id.clone(),
            None => own_host_id()?,
        };
        HostId::new(&id).map_err(|why| format!("--host-id: {why}"))?;
        let (login, key) = credential(params, root)?;
        HttpLease::connect(server, &login, params.ca_cert.as_deref(), &id, limit)
            .map(|client| (client, key))
            .map_err(|why| named(&why, key))
    }

    /// A connection that could not be made, for the reason `why`.
    fn failed(why: String) -> Self {
        Self {
            client: Err(why),
            token_file: false,
        }
    }

    fn client(&self) -> Result<&HttpLease, LeaseError> {
        self.client
            .as_ref()
            .map_err(|why| LeaseError::Unreachable(why.clone()))
    }

    /// `result`, with the refusal `host-credential-required` of a call made with the token file
    /// (there is no host credential in the state root) told with its cause.
    fn named<T>(&self, result: Result<T, LeaseError>) -> Result<T, LeaseError> {
        match result {
            Err(LeaseError::Refused(code))
                if self.token_file && code == HOST_CREDENTIAL_REQUIRED =>
            {
                Err(LeaseError::Refused(format!("{code}: {NO_HOST_CREDENTIAL}")))
            }
            other => other,
        }
    }
}

/// The server's refusal of a call to the own-host routes made with the pool's shared token.
const HOST_CREDENTIAL_REQUIRED: &str = "host-credential-required";

/// What the hook adds to that refusal when it sent the token file for want of a host credential.
const NO_HOST_CREDENTIAL: &str = "no host credential in the state root: this host enrolled with the shared pool token; re-enroll with a join token (docs/runbooks/farm-node-enroll.md)";

impl Lease for MachineLease {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        self.named(self.client()?.cordon(lease_secs))
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        self.named(self.client()?.uncordon())
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        self.named(self.client()?.host())
    }
}

/// How to reach the lease server: the server, the certificate to trust, the host certificate and
/// key, and the host id of the host's own agent flags (`flags_file`), read as `js-agent
/// --flags-file` reads them, with the state root and the token file of the params.
///
/// # Errors
/// The flags file cannot be used (see [`flags_file::expand_with`]), its flags are not the
/// agent's (the server and the pool are required, as for the agent), or its host id cannot be
/// resolved. The text starts with the params key, `flags_file`.
pub(super) fn connect_with(
    flags_file: &Path,
    state_root: &Path,
    token_file: &Path,
    machine: &Machine<'_>,
) -> Result<ConnectParams, String> {
    let cli = read_flags(flags_file, machine)?;
    connect_from(cli, state_root, token_file, machine)
}

/// The agent's flags as the flags file names them.
///
/// # Errors
/// The flags file cannot be used, or holds flags the agent does not take. The text starts with
/// the params key, `flags_file`.
fn read_flags(flags_file: &Path, machine: &Machine<'_>) -> Result<Cli, String> {
    let args = vec![
        "js-agent".into(),
        flags_file::FLAG.into(),
        flags_file.as_os_str().to_owned(),
    ];
    parse_agent_flags(args, machine.euid, machine.flags)
        .map_err(|why| format!("{FLAGS_FILE_KEY}: {why}"))
}

/// Whether the flags set an egress ceiling above `none` (`--egress allow-list` or `open`): an
/// agent started with one removes, at its start, every run of an earlier process that may still
/// run (their network wiring cannot be taken over), so an update cannot hand them over.
#[cfg(target_os = "linux")]
fn egress_of(cli: &Cli) -> bool {
    egress_set(cli.os.egress.as_deref())
}

/// Whether `--egress` given as `mode` (`None` when it is not given) sets a ceiling: anything but
/// `none`, a value the agent would refuse included (such an agent does not start).
#[cfg(target_os = "linux")]
fn egress_set(mode: Option<&str>) -> bool {
    mode.is_some_and(|mode| mode != "none")
}

/// Off Linux there is no `--egress`: the macOS build's network ceiling keeps no wiring that a
/// start removes.
#[cfg(not(target_os = "linux"))]
fn egress_of(_cli: &Cli) -> bool {
    false
}

/// The host's certificate and key when the flags name both. It does not depend on the host id:
/// a host id that cannot be resolved leaves the files named.
fn identity_of(cli: &Cli) -> Option<HostIdentity> {
    cli.host_cert
        .clone()
        .zip(cli.host_key.clone())
        .map(|(cert, key)| HostIdentity::new(cert, key))
}

/// The connection the flags `cli` describe.
///
/// # Errors
/// The host id cannot be resolved (`--host-id-from-facts` names a facts file that cannot be
/// used). The text starts with the params key, `flags_file`.
fn connect_from(
    cli: Cli,
    state_root: &Path,
    token_file: &Path,
    machine: &Machine<'_>,
) -> Result<ConnectParams, String> {
    let host_id = resolve_host_id(&cli, machine.euid, machine.flags)
        .map_err(|why| format!("{FLAGS_FILE_KEY}: {why}"))?;
    let host_identity = identity_of(&cli);
    Ok(ConnectParams {
        server: Some(cli.server),
        state_root: Some(state_root.to_owned()),
        token_file: Some(token_file.to_owned()),
        ca_cert: cli.ca_cert,
        host_id,
        host_identity,
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

impl Root {
    /// The state root when one is there: the one place an absent root is told apart, so
    /// that each view below delegates without an arm of its own.
    fn open(&self) -> Option<&StateRoot> {
        match self {
            Root::Open(root) => Some(root),
            Root::Absent => None,
        }
    }
}

impl StateView for Root {
    fn taint(&self) -> Option<Taint> {
        self.open().and_then(StateView::taint)
    }

    fn restored_rechecks(&self, taint: &Taint) -> bool {
        self.open()
            .is_some_and(|root| StateView::restored_rechecks(root, taint))
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        self.open().map_or_else(|| Ok(Vec::new()), StateView::owed)
    }

    fn has_credential(&self) -> bool {
        self.open().is_some_and(StateView::has_credential)
    }

    fn request_handover(&self) -> Result<(), PortError> {
        match self.open() {
            Some(root) => root.request_handover(),
            None => Err(PortError::Io("there is no state root".to_owned())),
        }
    }

    fn withdraw_handover(&self) -> Result<(), PortError> {
        self.open().map_or(Ok(()), StateView::withdraw_handover)
    }
}

/// The unit manager of this platform: on a Mac, [`launch_units`], gated on `token_file`.
#[cfg(target_os = "macos")]
pub(super) fn platform_units<'a>(
    commands: &'a SystemCommands,
    stop: &'a StopRunner,
    flags_file: &'a Path,
) -> PlatformUnits<'a> {
    launch_units(commands, stop, flags_file)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn platform_units<'a>(
    commands: &'a SystemCommands,
    _stop: &'a StopRunner,
    _flags_file: &'a Path,
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
/// longer: see [`StopRunner`]), and a kickstart held back while the host's flags file (the
/// `flags_file` param, which the plist's `KeepAlive` also waits on) is absent. The gate is the
/// flags file, not the join token: a Mac that joins by its host certificate has no token. Built for the
/// tests on every platform, so that a test off a Mac checks the wiring the Mac's hook runs with.
#[cfg(any(target_os = "macos", test))]
fn launch_units<'a>(
    commands: &'a dyn CommandRunner,
    stop: &'a dyn CommandRunner,
    flags_file: &'a Path,
) -> super::LaunchUnits<'a> {
    super::LaunchUnits::new(commands)
        .with_stop_runner(stop)
        .with_gate(flags_file)
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
    let units = platform_units(&commands, &stop, &params.flags_file);
    // A flags file that cannot be read names no host certificate: a first install then goes by
    // the join token as before, and a step that asks the lease server ends with the reason.
    let flags = read_flags(&params.flags_file, machine);
    let host_identity = flags.as_ref().ok().and_then(identity_of);
    // A flags file that cannot be read says nothing of the egress: it counts as one, and the
    // update drains as before.
    let egress = flags.as_ref().map_or(true, egress_of);
    let connect =
        flags.and_then(|cli| connect_from(cli, &params.state_root, &params.token_file, machine));
    let lease = match &connect {
        Ok(connect) => MachineLease::with_root(
            connect,
            Ok(&state),
            machine.own_host_id,
            RequestLimit::DEFAULT,
        ),
        Err(why) => MachineLease::failed(why.clone()),
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
        host_identity: host_identity.as_ref(),
        egress,
        stat: machine.stat,
        look: machine.look,
        fstab_leftover: machine.fstab_leftover,
    };
    match invocation.step {
        Step::Prepare => step::prepare(&env, params),
        Step::Cutover => step::cutover(&env, params),
        Step::Verify => step::verify(&env, params),
        Step::Rollback => step::rollback(&env, params),
        Step::Cleanup => step::cleanup(&env, params),
    }
}

/// The host facts that work on the host besides an update can change, as the agent reads them
/// when it starts (the CPUs, the memory, the GPUs, and on a Mac the major version of macOS and
/// the builds of the installed Xcodes, `xcode=16C5032a,16F6`, as [`crate::xcode::probe`] finds
/// them: every Xcode found, ready or not), as one line: equal facts give equal lines, and a
/// Mac that gains an Xcode gives a different line. Off a Mac the line has no `xcode` part and
/// nothing is listed. Whether an Xcode is ready, and its simulator runtimes
/// ([`crate::xcode::survey`]), are not part of the line: a change in those is advertised when
/// the agent next starts.
///
/// # Errors
/// A fact cannot be read (see [`detect`]).
pub fn host_facts(
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<String, String> {
    let detected = detect(platform, runner, files)?;
    let (os_version, _) = crate::os_version::report(platform, runner);
    let devices: Vec<String> = detected
        .gpus
        .devices
        .iter()
        .map(|gpu| {
            format!(
                "{}:{}:{}:{}",
                gpu.index, gpu.uuid, gpu.model, gpu.memory_bytes
            )
        })
        .collect();
    let mut line = format!(
        "os={} arch={} os_version={} cpu_millis={} memory_bytes={} gpus={} gpu_class={} gpu_devices={}",
        platform.os,
        platform.arch,
        os_version.unwrap_or_default(),
        detected.cpu_millis,
        detected.memory_bytes,
        detected.gpus.count,
        detected.gpus.class.unwrap_or_default(),
        devices.join(",")
    );
    if platform.is_macos() {
        let (xcodes, _) = crate::xcode::probe(files);
        line = format!("{line} xcode={}", crate::xcode::joined(&xcodes));
    }
    Ok(line)
}

/// [`Facts`] on a machine: [`host_facts`] of `platform`, read through `runner` and `files`.
pub struct SystemFacts<'a> {
    /// The platform the facts are read on.
    pub platform: Platform,
    /// Starts the programs the facts are read with.
    pub runner: &'a dyn CommandRunner,
    /// Reads the files the facts are read from.
    pub files: &'a dyn FileReader,
}

impl Facts for SystemFacts<'_> {
    fn read(&self) -> Result<String, String> {
        host_facts(&self.platform, self.runner, self.files)
    }
}

/// A maintenance step refused for its state root: `state_invalid`, with nothing changed.
fn invalid_root(detail: String) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(Reason::StateInvalid, detail),
    }
}

/// Runs the maintenance step the invocation names on this machine: the lease server the host's
/// flags file names, the hold file of the agent's state root, the platform's unit manager and
/// the machine's host facts.
#[must_use]
pub fn run_maintain(
    invocation: &MaintainInvocation,
    params: &DrainParams,
    machine: &Machine<'_>,
) -> Done {
    let state = match find_root(Some(&params.state_root)) {
        Ok(state) => state,
        Err(why) => return invalid_root(why.to_string()),
    };
    let Some(root) = state.open() else {
        return match invocation.step {
            MaintenanceStep::Drain => invalid_root(format!(
                "{STATE_ROOT_KEY}: {} is not there, and a drain needs the state root of the agent that runs",
                params.state_root.display()
            )),
            // No agent ever ran here, so none was held: nothing to put back.
            MaintenanceStep::Resume => Done {
                outcome: Outcome::Success,
                report: Report::default(),
            },
        };
    };
    let commands = SystemCommands::default();
    let stop = StopRunner::new(Duration::from_secs(u64::from(AGENT_EXIT_TIMEOUT_SECS)));
    let units = platform_units(&commands, &stop, &params.token_file);
    let lease = match connect_with(
        &params.flags_file,
        &params.state_root,
        &params.token_file,
        machine,
    ) {
        Ok(connect) => MachineLease::with_root(
            &connect,
            Ok(&state),
            machine.own_host_id,
            RequestLimit::DEFAULT,
        ),
        Err(why) => MachineLease::failed(why),
    };
    let facts = SystemFacts {
        platform: Platform::current(),
        runner: &commands,
        files: &SystemFiles,
    };
    let env = MaintainEnv {
        lease: &lease,
        holds: root,
        units: &units,
        facts: &facts,
        clock: &SystemClock,
        gate: &NoGate,
    };
    match invocation.step {
        MaintenanceStep::Drain => step::drain(&env, params),
        MaintenanceStep::Resume => step::resume(&env, params),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use js_client::ClientError;

    use super::{CommandRunner, Root, StateView, Taint, launch_units, named};
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
    /// kickstart waits on the host's flags file. It catches units built without the stop
    /// runner or the gate, or with the two runners swapped or one runner for both.
    #[test]
    fn the_units_of_a_mac_read_through_the_commands_stop_through_the_stop_runner_and_wait_on_the_flags()
     {
        let label = "com.example.agent";
        let flags = Path::new("/etc/example/flags");
        let print = format!("{LAUNCHCTL} print system/{label}");
        let commands = Noting::default();
        let stop = Noting::default();
        let units = launch_units(&commands, &stop, flags);
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
        assert_eq!(units.gate(), Some(flags));
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

    /// Asserts that a state root that is not there (a first install) holds no taint, re-checks
    /// none, and owes no check. It catches an absent root read as tainted, or as one whose
    /// taint a restart re-checks, which would let the hook go on over a taint it never read.
    #[test]
    fn an_absent_state_root_holds_no_taint_rechecks_none_and_owes_nothing() {
        let root = Root::Absent;
        let taint = Taint {
            reason: "residue".to_owned(),
            run_id: None,
            check: Some("daemonised-child".to_owned()),
        };
        assert_eq!(root.taint(), None);
        assert!(!root.restored_rechecks(&taint));
        assert_eq!(root.owed().unwrap(), Vec::<String>::new());
    }

    /// Asserts which `--egress` values of the flags file keep an update from handing the runs
    /// over: `allow-list`, `open` and anything else but `none`; not the flag left out, nor
    /// `none`. It catches an agent with a ceiling asked to hand over, whose next process would
    /// remove the runs it was to adopt.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_egress_but_none_keeps_an_update_from_handing_over() {
        use super::egress_set;
        assert!(!egress_set(None));
        assert!(!egress_set(Some("none")));
        assert!(egress_set(Some("allow-list")));
        assert!(egress_set(Some("open")));
        assert!(egress_set(Some("other")));
    }

    /// Asserts the handover request through the hook's state root: with no state root it
    /// cannot be made (the error says why) and withdrawing it is a no-op; with one, the request
    /// writes the handover file and the withdrawal removes it, twice over. It catches a request
    /// reported as made where no agent could read it.
    #[test]
    fn a_handover_is_asked_in_the_state_root_and_cannot_be_without_one() {
        let absent = Root::Absent;
        assert_eq!(
            absent.request_handover().unwrap_err().to_string(),
            "there is no state root"
        );
        assert!(absent.withdraw_handover().is_ok());

        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "hook-root-handover");
        let path = dir.path().join("state");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let open = Root::Open(crate::lifecycle::StateRoot::open(&path).unwrap());
        let file = path.join(crate::lifecycle::handover::HANDOVER_FILE);
        open.request_handover().unwrap();
        assert!(file.is_file());
        open.withdraw_handover().unwrap();
        assert!(!file.exists());
        open.withdraw_handover().unwrap();
    }
}
