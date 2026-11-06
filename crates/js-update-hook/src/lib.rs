//! What every service's side of the host updater's hook contract shares (host-updates design,
//! 4.2), whatever the service: how a step ends ([`Outcome`], [`Report`]), the five steps by
//! name ([`Step`]), the locked state directory and its record ([`Held`]), the unit managers
//! ([`SystemUnits`] on Linux, [`LaunchUnits`] on a Mac), the release link ([`Link`],
//! [`swap_link`]), time ([`Clock`]), the kill points ([`Gate`]), the spelling rule for a
//! trusted path ([`is_plain`]) and the bounds the steps' waits are built from ([`POLL_SECS`]
//! and the margins).
//!
//! The updater runs one step per process and reads two things: the exit status and the last
//! line of standard output, a JSON object ([`Report`]). Exit 0 is success, 1 failure, 75
//! deferred (nothing irreversible happened, try later) and 76 halt (no version can repair
//! this).
//!
//! A hook names its failures with a closed set of its own ([`ReasonToken`]) and keeps its
//! record as a [`Document`] of its own; the rest is here. Programs are run through a
//! [`CommandRunner`] the hook gives, so a test feeds recorded output instead of touching the
//! machine. Nothing here reads an environment variable.

mod launch_units;
mod outcome;
mod plain;
mod ports;
mod report;
mod runner;
mod state;
mod step;
mod timing;

pub use launch_units::{LAUNCHCTL, LaunchUnits, SETTLE_READS};
pub use outcome::Outcome;
pub use plain::is_plain;
pub use ports::{
    Clock, Gate, Link, NoGate, PortError, SYSTEMCTL, SystemClock, SystemLink, SystemUnits,
    UnitState, Units, read_link, swap_link,
};
pub use report::{Class, MAX_REASON_LEN, ReasonToken, Report, is_reason_token};
pub use runner::{CommandRunner, Streams};
pub use state::{
    DirProblem, DirStat, Document, Held, LOCK_FILE, RECORD_FILE, RECORD_MAX, StateError, TEMP_FILE,
    check_ancestor, check_dir, fd_stat, peek,
};
pub use step::{Step, UnknownStep};
pub use timing::{
    CUTOVER_MARGIN_SECS, POLL_SECS, PREPARE_MARGIN_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    VERIFY_MARGIN_SECS,
};
