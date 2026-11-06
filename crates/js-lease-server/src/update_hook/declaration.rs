//! The hook's declaration, `/etc/updater.d/js-lease-server.toml`: the part of it the hook
//! reads, and the bounds its step timeouts are held to.
//!
//! ```toml
//! state_dir = "/var/lib/js-lease-server-update"
//!
//! [hook]
//! units       = ["js-lease-server.service"]
//! write_paths = ["/opt/js-lease-server/releases", "/opt/js-lease-server/link", "/var/lib/js-lease-server-update"]
//!
//! [hook.timeout_secs]
//! prepare = 90
//! # cutover, verify, rollback, cleanup likewise
//! ```
//!
//! Keys the hook does not read are ignored: the file is the updater's, and it adds keys.
//!
//! **The timeouts.** Each step's waits are built from the constants here, and the declared
//! timeout of a step must be at least the sum ([`MIN_TIMEOUTS`]), so the updater never kills a
//! step in the middle of a wait. There is no drain term: an update restarts the server in
//! place, and a running job is not waited for.

use std::io;
use std::path::{Path, PathBuf};

use js_update_hook::{
    CUTOVER_MARGIN_SECS, PREPARE_MARGIN_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    VERIFY_MARGIN_SECS, is_plain,
};
use serde::Deserialize;

/// Where the declaration is installed.
pub const DECLARATION_PATH: &str = "/etc/updater.d/js-lease-server.toml";

/// How long a stopped server has to exit, in seconds: systemd's default `TimeoutStopSec`.
pub const STOP_WAIT_SECS: u64 = 90;

/// How long the store's writer lock has to be let go once the server is down, in seconds. A
/// Postgres advisory lock ends with its session, which ends when the process's connection
/// closes.
pub const LOCK_WAIT_SECS: u64 = 30;

/// How long `verify` looks for the new server, in seconds, before its margin.
pub const VERIFY_WAIT_SECS: u64 = 285;

/// How long `rollback` waits for the previous release to take the writer lock, in seconds.
pub const SERVE_WAIT_SECS: u64 = 120;

/// How long `prepare`'s reads of the store take at most, in seconds: the schema version and
/// the probe of the writer lock, each on a connection bounded by the store's connect limit.
pub const PREPARE_WORK_SECS: u64 = 30;

/// How long `cleanup` takes at most, in seconds: the lock of the state directory and a file
/// delete.
pub const CLEANUP_SECS: u64 = 60;

/// Why a declaration was refused.
#[derive(Debug, thiserror::Error)]
pub enum DeclarationError {
    /// The file could not be read.
    #[error("cannot read the declaration: {0}")]
    Read(#[from] io::Error),
    /// The file is not TOML, or lacks a key the hook needs, or has one of the wrong type.
    #[error("the declaration is not usable: {0}")]
    Parse(#[from] toml::de::Error),
    /// `state_dir` or a write path is not absolute and plain (no `.`, `..`, doubled or
    /// trailing slash).
    #[error("state_dir and every write path must be absolute paths of plain components")]
    NotPlain,
    /// `state_dir` is not one of `write_paths`: the hook could not write its own record.
    #[error("state_dir is not listed in hook.write_paths")]
    StateDirNotListed,
    /// A write path lies inside another, or is listed twice: a release, or the server, could
    /// then reach the record or the link.
    #[error("hook.write_paths {inner} lies inside {outer}")]
    Nested {
        /// The path inside.
        inner: String,
        /// The path it lies in.
        outer: String,
    },
    /// `hook.units` does not name exactly one unit: the hook manages one server.
    #[error("hook.units must name exactly one unit, not {0}")]
    Units(usize),
    /// A step's timeout is shorter than the waits the step is built from.
    #[error("the declared {step} timeout {declared} s is shorter than the hook's {min} s")]
    Timeout {
        /// The step.
        step: &'static str,
        /// What the declaration says.
        declared: u64,
        /// [`MIN_TIMEOUTS`]'s.
        min: u64,
    },
    /// `verify_deadline_secs` is shorter than the `verify` bound plus the `rollback` bound: the
    /// updater would roll back a verify that is still allowed to run.
    #[error(
        "verify_deadline_secs {deadline} is less than the verify bound {verify} plus the rollback bound {rollback}"
    )]
    VerifyDeadline {
        /// `verify_deadline_secs`.
        deadline: u64,
        /// `[hook.timeout_secs] verify`.
        verify: u64,
        /// `[hook.timeout_secs] rollback`.
        rollback: u64,
    },
}

/// The `[hook.timeout_secs]` table: the updater's bound on each step, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Timeouts {
    /// `prepare`.
    pub prepare: u64,
    /// `cutover`.
    pub cutover: u64,
    /// `verify`.
    pub verify: u64,
    /// `rollback`.
    pub rollback: u64,
    /// `cleanup`.
    pub cleanup: u64,
}

impl Timeouts {
    /// The five, by step name, in the order of the contract's table.
    #[must_use]
    pub fn by_step(&self) -> [(&'static str, u64); 5] {
        [
            ("prepare", self.prepare),
            ("cutover", self.cutover),
            ("verify", self.verify),
            ("rollback", self.rollback),
            ("cleanup", self.cleanup),
        ]
    }
}

/// The shortest timeout of each step: the sum of the waits it is built from.
///
/// - `prepare`: the store's reads and a margin;
/// - `cutover`: the stop, the wait for the writer lock to be let go, the start, a margin;
/// - `verify`: the looks and a margin;
/// - `rollback`: the stop, the lock's release, the start, the wait for the previous release
///   to take the lock, a margin;
/// - `cleanup`: the delete of the record.
pub const MIN_TIMEOUTS: Timeouts = Timeouts {
    prepare: PREPARE_WORK_SECS + PREPARE_MARGIN_SECS,
    cutover: STOP_WAIT_SECS + LOCK_WAIT_SECS + START_WAIT_SECS + CUTOVER_MARGIN_SECS,
    verify: VERIFY_WAIT_SECS + VERIFY_MARGIN_SECS,
    rollback: STOP_WAIT_SECS
        + LOCK_WAIT_SECS
        + START_WAIT_SECS
        + SERVE_WAIT_SECS
        + ROLLBACK_MARGIN_SECS,
    cleanup: CLEANUP_SECS,
};

#[derive(Deserialize)]
struct Wire {
    state_dir: PathBuf,
    verify_deadline_secs: Option<u64>,
    hook: WireHook,
}

#[derive(Deserialize)]
struct WireHook {
    units: Vec<String>,
    write_paths: Vec<PathBuf>,
    timeout_secs: Timeouts,
}

/// What the hook reads of its declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The directory of the hook record: `root:root`, mode 0700.
    pub state_dir: PathBuf,
    /// The one unit the hook may manage.
    pub unit: String,
    /// Every directory the hook may write, the state directory among them, none inside
    /// another.
    pub write_paths: Vec<PathBuf>,
    /// The bound on each step.
    pub timeouts: Timeouts,
}

impl Declaration {
    /// Reads the declaration at `path`.
    ///
    /// # Errors
    /// [`DeclarationError`]: unreadable, or not a usable declaration.
    pub fn read(path: &Path) -> Result<Declaration, DeclarationError> {
        Declaration::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a declaration.
    ///
    /// # Errors
    /// [`DeclarationError`]: see the variants.
    pub fn parse(text: &str) -> Result<Declaration, DeclarationError> {
        let wire: Wire = toml::from_str(text)?;
        let timeouts = wire.hook.timeout_secs;
        let paths = wire.hook.write_paths;
        if !is_plain(&wire.state_dir) || !paths.iter().all(|path| is_plain(path)) {
            return Err(DeclarationError::NotPlain);
        }
        if !paths.contains(&wire.state_dir) {
            return Err(DeclarationError::StateDirNotListed);
        }
        for (i, inner) in paths.iter().enumerate() {
            let outer = paths
                .iter()
                .enumerate()
                .find(|(j, outer)| *j != i && inner.starts_with(outer));
            if let Some((_, outer)) = outer {
                return Err(DeclarationError::Nested {
                    inner: inner.display().to_string(),
                    outer: outer.display().to_string(),
                });
            }
        }
        let [unit] = <[String; 1]>::try_from(wire.hook.units)
            .map_err(|units| DeclarationError::Units(units.len()))?;
        for ((step, declared), (_, min)) in
            timeouts.by_step().into_iter().zip(MIN_TIMEOUTS.by_step())
        {
            if declared < min {
                return Err(DeclarationError::Timeout {
                    step,
                    declared,
                    min,
                });
            }
        }
        // The updater rolls back a verify that has not succeeded by the deadline: it must not
        // do that to one that is still allowed to run, and the rollback needs its own bound.
        if let Some(deadline) = wire.verify_deadline_secs
            && deadline < timeouts.verify.saturating_add(timeouts.rollback)
        {
            return Err(DeclarationError::VerifyDeadline {
                deadline,
                verify: timeouts.verify,
                rollback: timeouts.rollback,
            });
        }
        Ok(Declaration {
            state_dir: wire.state_dir,
            unit,
            write_paths: paths,
            timeouts,
        })
    }
}
