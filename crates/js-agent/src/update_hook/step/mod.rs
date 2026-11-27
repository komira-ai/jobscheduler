//! The steps of the hook: `prepare`, `cutover`, `verify`, `rollback` and `cleanup`
//! (host-updates design, 4.2), and the two steps of a maintenance run, `drain` and `resume`
//! (hostkeeper's maintenance-updates design, 5.2), which take a [`MaintainEnv`] of their own and
//! keep no record: what they did is the hold file of the agent's state root and the cordon.
//!
//! A step is a function of an [`Env`] and the params table, and ends as a [`Done`]: the status
//! and the last line. It takes the state directory's lock first (making the directory when it
//! is not there), loads and validates the record, and only then looks at the machine. The
//! exceptions are the record of another target: `prepare` may read the unit's state, the release
//! link, the handover file and the lease server to decide whether that record is superseded,
//! finished or refused, and `rollback` rolls it back with the params it was written under (see
//! [`supersede`]). Every
//! change to the record or through a port passes a kill point first
//! ([`Ports::point`](super::Ports::point)), so that a test, and later the conformance harness,
//! can end the step at any of them and run it again.

mod cleanup;
mod cutover;
mod drain;
mod held;
mod helper;
mod host_setup;
mod in_place;
mod pf;
mod prepare;
mod resume;
mod rollback;
mod supersede;
mod verify;

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use js_client::HostIdentity;

use crate::facts::CommandRunner;

use super::{
    Class, Clock, DirStat, Done, Facts, Gate, Held, Holds, Lease, LeaseError, Manager, Outcome,
    Params, PortError, Ports, Reason, Record, Report, StateError, Step, Units, validate,
};

pub use cleanup::cleanup;
pub use cutover::cutover;
pub(crate) use cutover::{Life, life};
pub use drain::drain;
pub use host_setup::{
    DATA_VOLUME, DISKUTIL, DSCL, DSEDITGROUP, FSTAB_LEFTOVER, MOUNT_POINT_MODE, QUOTA_TENTHS,
    SCRATCH_VOLUME,
};
pub(crate) use prepare::neighbours_missing;
pub use prepare::prepare;
pub use resume::{RESTART_HELLO_SECS, RESTART_STOP_SECS, resume};
pub use rollback::rollback;
pub use verify::verify;

/// What the hook finds when it looks at a secret file: the join token file, or a file of the
/// host's certificate and key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenFacts {
    /// Whether it is a regular file after following links.
    pub regular: bool,
    /// Its permission bits.
    pub mode: u32,
    /// Its owner.
    pub uid: u32,
}

/// How the token file and the host certificate and key files are looked at (see
/// [`TokenFacts`]).
pub type TokenFn = dyn Fn(&Path) -> io::Result<TokenFacts>;

/// What the host setup of a Mac finds when it looks at a root it made (a link followed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathFacts {
    /// The device number of the file system that holds it (`st_dev`).
    pub device: u64,
    /// Its owner.
    pub uid: u32,
}

/// How the host setup looks at the state root and the scratch root (see [`PathFacts`]).
pub type LookFn = dyn Fn(&Path) -> io::Result<PathFacts>;

/// Everything a step touches.
pub struct Env<'a> {
    /// The unit manager, the release link, the lease server, the agent's state root, time and
    /// the kill points.
    pub ports: Ports<'a>,
    /// Runs the staged agent's `--version`, the staged `js-mac-provision` that loads the pool's
    /// pf anchor on a Mac, `launchctl bootstrap`, and `systemctl` where the unit manager is
    /// systemd.
    pub runner: &'a dyn CommandRunner,
    /// Which unit manager runs the agent, and so which unit file a release carries.
    pub manager: Manager,
    /// The user the hook runs as: the owner the join token file must have.
    pub euid: u32,
    /// How the join token file and the host certificate and key files are looked at.
    pub token: &'a TokenFn,
    /// The host certificate and key the host's flags file (`flags_file`) names, when it names
    /// both; `None` when it names neither or cannot be read. A first install that has them
    /// needs no join token (see `prepare`).
    pub host_identity: Option<&'a HostIdentity>,
    /// The host's flags file sets an egress ceiling (`--egress allow-list` or `open`), or
    /// cannot be read: the runs have network wiring, so an update hands them over only between
    /// agents that both list `egress-handover` on their `stops:` line, and drains the host
    /// otherwise (see `prepare`).
    pub egress: bool,
    /// The host's flags file turns the VM backend on (`--vm-driver`, a macOS flag), or cannot be
    /// read: the agent never adopts a VM run, so an update on launchd drains the host (see
    /// `prepare`). Always `false` in a Linux build, which has no such flag.
    pub vms: bool,
    /// How a directory of the state directory's path is looked at (see
    /// [`fd_stat`](super::fd_stat)).
    pub stat: &'a dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
    /// How the host setup of a Mac looks at the state root and the scratch root.
    pub look: &'a LookFn,
    /// The copy of the file system table an earlier release's host setup left beside it, which
    /// the host setup removes: [`FSTAB_LEFTOVER`] on a Mac.
    pub fstab_leftover: &'a Path,
}

/// Everything `drain` and `resume` touch: the lease server's own-host routes, the hold file
/// of the agent's state root, the unit manager and the host facts (`resume` restarts the agent
/// when the facts changed), time and the kill points.
pub struct MaintainEnv<'a> {
    /// The lease server.
    pub lease: &'a dyn Lease,
    /// The hold file.
    pub holds: &'a dyn Holds,
    /// The unit manager.
    pub units: &'a dyn Units,
    /// The host facts.
    pub facts: &'a dyn Facts,
    /// Time.
    pub clock: &'a dyn Clock,
    /// Kill points: one before each change.
    pub gate: &'a dyn Gate,
}

/// Whether `step` on a host whose record is `record` is past its commit, so that every ending
/// of it says `reversible: false`: `cutover` and `rollback` once the first irreversible act is
/// begun (the stop, or a first install's move of the link; see `cutover`).
pub(crate) fn committed(step: Step, record: &Record) -> bool {
    matches!(step, Step::Cutover | Step::Rollback) && cutover::begun(record)
}

/// A step that ended without a body of its own to say more: success, nothing to tell.
fn success() -> Done {
    Done {
        outcome: Outcome::Success,
        report: Report::default(),
    }
}

/// A failure of the step's own: exit 1, a `release` matter.
fn failure(reason: Reason, detail: impl Into<String>) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(reason, detail),
    }
}

/// The state directory, the lock or the record is not as the hook requires.
fn state_failure(error: StateError) -> Done {
    failure(error.reason(), error.to_string())
}

/// The lease server did not answer as asked: not the release's fault.
fn lease_failure(error: LeaseError) -> Done {
    let mut done = failure(Reason::LeaseUnreachable, error.to_string());
    done.report.class = Class::Environment;
    done
}

/// The hold file could not be read, written or ended: the state root's matter, not the
/// release's.
fn hold_failure(error: PortError) -> Done {
    let mut done = failure(Reason::HoldFailed, format!("the hold file: {error}"));
    done.report.class = Class::Environment;
    done
}

/// Takes the lock, loads the record and holds it to the rules; `None` if there is none. The
/// state directory is made (mode 0700) when it is not there, with every directory above it that
/// is not there (mode 0755): hostkeeper makes none, so the first step of a first install finds
/// none (see [`Held::open_or_make`]).
fn open_state(env: &Env<'_>, params: &Params) -> Result<(Held, Option<Record>), Done> {
    let (held, record) = open_unchecked(env, params)?;
    if let Some(record) = &record {
        check(params, record)?;
    }
    Ok((held, record))
}

/// [`open_state`] without the rules: the record as it was loaded, for `prepare` and `rollback`
/// to judge themselves (see [`supersede`]).
fn open_unchecked(env: &Env<'_>, params: &Params) -> Result<(Held, Option<Record>), Done> {
    helper::refuse_without_launchd(env, params)?;
    pf::refuse_without_launchd(env, params)?;
    let held = Held::open_or_make(&params.settings.state_dir, env.stat).map_err(state_failure)?;
    let record = held.load().map_err(state_failure)?;
    Ok((held, record))
}

/// Holds `record` to the rules of the params table.
fn check(params: &Params, record: &Record) -> Result<(), Done> {
    validate(record, params)
        .map_err(StateError::Invalid)
        .map_err(state_failure)
}

/// Makes `record` the record, after a kill point.
fn save(env: &Env<'_>, held: &Held, record: &Record) -> Result<(), Done> {
    env.ports.point();
    held.save(record).map_err(state_failure)
}
