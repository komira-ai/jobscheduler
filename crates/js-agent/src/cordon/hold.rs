//! The window: cordon, renew, watch for idle, lift. Everything outside the program (the
//! server, time, the stop request, standard output) is given, so the whole of it runs in a
//! test with no wait.

use std::io::Write;
use std::time::Duration;

use super::Pace;
use crate::own_host::Lease;

/// How often the host is read while it is not yet idle, so that a script waiting for the
/// `idle` line is told within this long of the last lease ending, whatever the renewal
/// interval is.
pub const IDLE_POLL: Duration = Duration::from_secs(5);

/// What to hold and for how long.
#[derive(Clone, Copy, Debug)]
pub struct Plan<'a> {
    /// The host to cordon. Every reply of the server must name it.
    pub host: &'a str,
    /// The length of each cordon request, in seconds (1 to `MAX_CORDON_SECS`).
    pub lease_secs: u64,
    /// How long to hold the cordon.
    pub window: Duration,
}

/// How the window ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The cordon was held to the end of the window or a stop request, and lifted (or had
    /// lapsed).
    Done,
    /// The host was cordoned already when the verb asked: it is another holder's, and it is
    /// left as it is.
    OtherHolder,
    /// The server or the credential failed; the text says which, and never holds the
    /// credential.
    Failed(String),
}

/// Cordons the host, holds the cordon for the window and lifts it, and prints a line for
/// each state change on `out`.
///
/// The cordon is lifted only if this call placed it (the first reply said `already_cordoned:
/// false`) and every reply named the host that was asked for: the server cannot say who
/// placed a cordon, and an uncordon lifts any. Time passes only in `pace`, which also tells
/// of a stop request; a stop lifts the cordon like the end of the window does.
pub fn hold(
    _lease: &dyn Lease,
    _pace: &mut dyn Pace,
    _plan: &Plan<'_>,
    _out: &mut dyn Write,
) -> Outcome {
    Outcome::Failed("not implemented".to_owned())
}
