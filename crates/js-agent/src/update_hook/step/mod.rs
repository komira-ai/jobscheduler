//! The steps of the hook that have a body: `prepare` and `cleanup` (host-updates design, 4.2).

mod cleanup;
mod prepare;

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use crate::facts::CommandRunner;

use super::{Declaration, DirStat, Ports};

pub use cleanup::cleanup;
pub use prepare::prepare;

/// Everything a step touches.
pub struct Env<'a> {
    /// The unit manager, the release link, the lease server, the agent's state root, time and
    /// the kill points.
    pub ports: Ports<'a>,
    /// Runs the staged agent's `--version`.
    pub runner: &'a dyn CommandRunner,
    /// The hook's declaration.
    pub declaration: &'a Declaration,
    /// How a directory of the state directory's path is looked at (see
    /// [`fd_stat`](super::fd_stat)).
    pub stat: &'a dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
}
