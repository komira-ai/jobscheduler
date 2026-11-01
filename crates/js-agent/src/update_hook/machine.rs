//! The hook on this machine: the ports over the real unit manager, release link, lease server
//! and agent state, and the steps run over them. No body yet.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use crate::own_host::{Cordoned, HostReading, HttpLease, Lease, LeaseError, Uncordoned};

use super::{ConnectParams, DirStat, Done, Invocation, Params, Reason, Report};

/// How a directory of the state directory's path is looked at (see [`fd_stat`](super::fd_stat)).
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

impl Machine<'static> {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            declaration: Path::new("/"),
            stat: &|_, _| Err(io::Error::other("not implemented")),
            own_host_id: &|| Err("not implemented".to_owned()),
        }
    }
}

/// The lease server's own-host routes for the host the params name; a connection that could not
/// be made is every call's error.
pub struct MachineLease {
    client: Result<HttpLease, String>,
}

impl MachineLease {
    /// Connects as the params say.
    #[must_use]
    pub fn connect(
        _connect: &ConnectParams,
        _own_host_id: &dyn Fn() -> Result<String, String>,
    ) -> Self {
        Self {
            client: Err("not implemented".to_owned()),
        }
    }
}

impl Lease for MachineLease {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        match &self.client {
            Ok(client) => client.cordon(lease_secs),
            Err(why) => Err(LeaseError::Unreachable(why.clone())),
        }
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        match &self.client {
            Ok(client) => client.uncordon(),
            Err(why) => Err(LeaseError::Unreachable(why.clone())),
        }
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        match &self.client {
            Ok(client) => client.host(),
            Err(why) => Err(LeaseError::Unreachable(why.clone())),
        }
    }
}

/// Runs the step the invocation names on this machine.
#[must_use]
pub fn run_step(invocation: &Invocation, _params: &Params, _machine: &Machine<'_>) -> Done {
    Done {
        outcome: super::Outcome::Failure,
        report: Report::failure(
            Reason::NotImplemented,
            format!("{} is not implemented", invocation.step),
        ),
    }
}
