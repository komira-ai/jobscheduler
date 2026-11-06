//! The rules every value read from the record is held to before it is used (host-updates
//! design, 4.2, "Every value read from the record is validated against the settings").
//!
//! The record is the hook's own memory, kept where only root writes; the rules are for the
//! day that stops being true (a corrupt file, a record of another version, a directory that
//! was not what it should be). A record that fails one is not acted on: the step exits 1 with
//! the reason `state_invalid` and changes nothing. Where the record and the system disagree
//! about what was done, the system is believed; that is a step's decision and not a rule here.

use std::ffi::OsStr;
use std::path::Path;

pub use js_update_hook::{
    CUTOVER_MARGIN_SECS, PREPARE_MARGIN_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    VERIFY_MARGIN_SECS,
};
use js_update_hook::{POLL_SECS, is_plain};

use super::params::Params;
use super::record::{Record, SCHEMA};

/// The longest version string a record may hold.
pub const MAX_VERSION_LEN: usize = 128;

/// How far past `first_ms + cordon_max_secs * 1000` a recorded cordon expiry may lie, in
/// milliseconds. An expiry is a whole number of seconds while the first placement is a
/// millisecond stamp, so the latest legal expiry is the first stamp plus the cap plus at most
/// one second.
pub const EXPIRY_SLACK_MS: u64 = 1_000;

/// The most changes a unit restart plan may name.
pub const MAX_CHANGES: usize = 64;

/// The longest name of a change in a unit restart plan, in bytes.
pub const MAX_CHANGE_LEN: usize = 256;

/// How long `rollback` waits for the previous agent's hello once the unit is active, in
/// seconds.
pub const ROLLBACK_HELLO_SECS: u64 = 300;

/// The rule a record broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// The `prepare` timeout would end the step in the middle of the drain wait.
    #[error(
        "the prepare timeout {prepare} s is shorter than drain_wait_secs {wait} s plus {PREPARE_MARGIN_SECS} s"
    )]
    PrepareTimeout {
        /// `prepare_timeout_secs`.
        prepare: u64,
        /// `drain_wait_secs`.
        wait: u64,
    },
    /// The `cutover` timeout leaves no time to wait for the stop.
    #[error(
        "the cutover timeout {cutover} s leaves no time to stop: it must exceed {START_WAIT_SECS} s plus {CUTOVER_MARGIN_SECS} s"
    )]
    CutoverTimeout {
        /// `cutover_timeout_secs`.
        cutover: u64,
    },
    /// The `verify` timeout leaves no time for a look and a poll.
    #[error(
        "the verify timeout {verify} s is too short: it must exceed {VERIFY_MARGIN_SECS} s plus {POLL_SECS} s"
    )]
    VerifyTimeout {
        /// `verify_timeout_secs`.
        verify: u64,
    },
    /// The `rollback` timeout would end the step in the middle of its waits.
    #[error(
        "the rollback timeout {rollback} s is too short: it must exceed drain_wait_secs {wait} s plus {START_WAIT_SECS} s, {ROLLBACK_HELLO_SECS} s and {ROLLBACK_MARGIN_SECS} s"
    )]
    RollbackTimeout {
        /// `rollback_timeout_secs`.
        rollback: u64,
        /// `drain_wait_secs`.
        wait: u64,
    },
    /// The record's schema is not the one this hook writes.
    #[error("schema {found} is not {SCHEMA}")]
    Schema {
        /// The record's.
        found: u32,
    },
    /// A release path is not an absolute path of plain components (no `.`, `..`, doubled or
    /// trailing slash), is the root, or lies in the state directory.
    #[error("{field} is not an absolute path of plain components outside the state directory")]
    Path {
        /// The record field.
        field: &'static str,
    },
    /// The record's unit is not the one the settings name.
    #[error("unit {found:?} is not the hook's unit")]
    Unit {
        /// The record's.
        found: String,
    },
    /// A version is empty, too long, or holds a character a version does not.
    #[error(
        "{field} is not a version of letters, digits and . _ + ~ - (at most {MAX_VERSION_LEN} bytes)"
    )]
    Version {
        /// The record field.
        field: &'static str,
    },
    /// A value of the record is not the one the updater passed in the params table.
    #[error("{field} of the record is not what the params table says")]
    ParamsMismatch {
        /// The record field.
        field: &'static str,
    },
    /// Exactly one of the previous version and the previous release is present.
    #[error(
        "the previous version and the previous release must both be present or both absent in the {side}"
    )]
    Unpaired {
        /// `record` or `params`.
        side: &'static str,
    },
    /// The staged release and the previous release are the same directory.
    #[error("the staged release and the previous release are the same")]
    SameRelease,
    /// The target and the previous version are the same: there is nothing to update.
    #[error("target and previous version are the same")]
    SameVersion,
    /// The artifact digest is not `sha256:` and 64 lowercase hex digits.
    #[error("artifact_digest is not sha256: and 64 lowercase hex digits")]
    Digest,
    /// The cordon's stamps contradict each other or its `placed` mark.
    #[error("the cordon's placed mark and its stamps disagree")]
    CordonStamps,
    /// The cordon would expire later than the declared cap allows.
    #[error("cordon expiry {expires_ms} is past the cap {limit_ms}")]
    CordonExpiry {
        /// The record's.
        expires_ms: u64,
        /// The latest the cap allows.
        limit_ms: u64,
    },
    /// The record's unit restart plan names no change, or a change that is not a plain name
    /// (empty, longer than [`MAX_CHANGE_LEN`] bytes or with a control character), or too many.
    #[error("the unit restart plan is not a list of up to {MAX_CHANGES} plain names")]
    UnitRestart,
    /// A mark is set without the one it follows from.
    #[error("{mark} is set but {needs} is not")]
    Marks {
        /// The mark that is set.
        mark: &'static str,
        /// The mark it needs.
        needs: &'static str,
    },
}

/// A release path: plain, not the root, and not in the state directory.
fn release_path(field: &'static str, path: &Path, state_dir: &Path) -> Result<(), Invalid> {
    let usable = is_plain(path) && path.parent().is_some() && !path.starts_with(state_dir);
    usable.then_some(()).ok_or(Invalid::Path { field })
}

/// The record's value is the params table's, spelt the same: compared as the bytes they are,
/// so `/opt/x` and `/opt/x/` are two values.
fn agrees(
    field: &'static str,
    record: Option<&OsStr>,
    params: Option<&OsStr>,
) -> Result<(), Invalid> {
    if record == params {
        Ok(())
    } else {
        Err(Invalid::ParamsMismatch { field })
    }
}

/// A version string.
fn is_version(text: &str) -> bool {
    (1..=MAX_VERSION_LEN).contains(&text.len())
        && !text.starts_with('-')
        && !text.contains("..")
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'~' | b'-'))
}

/// A digest as the registry writes it.
pub(super) fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Runs every rule on `record`.
///
/// # Errors
/// The first [`Invalid`] rule the record breaks.
pub fn validate(record: &Record, params: &Params) -> Result<(), Invalid> {
    let settings = &params.settings;
    let update = &record.update;
    // The updater kills a step at its declared bound: `prepare` must outlast the wait.
    let needed = params.drain_wait_secs.saturating_add(PREPARE_MARGIN_SECS);
    if settings.timeouts.prepare < needed {
        return Err(Invalid::PrepareTimeout {
            prepare: settings.timeouts.prepare,
            wait: params.drain_wait_secs,
        });
    }
    // The wait for the stop ends before the declared bound by the start wait and a margin.
    if settings.timeouts.cutover <= START_WAIT_SECS + CUTOVER_MARGIN_SECS {
        return Err(Invalid::CutoverTimeout {
            cutover: settings.timeouts.cutover,
        });
    }
    // Less and the wait would be a single look, with no poll after it.
    if settings.timeouts.verify <= VERIFY_MARGIN_SECS + POLL_SECS {
        return Err(Invalid::VerifyTimeout {
            verify: settings.timeouts.verify,
        });
    }
    // The waits of `rollback`: the drain, the stop, the start and the hello, and a margin.
    let rollback_needed = params
        .drain_wait_secs
        .saturating_add(START_WAIT_SECS)
        .saturating_add(ROLLBACK_HELLO_SECS)
        .saturating_add(ROLLBACK_MARGIN_SECS);
    if settings.timeouts.rollback <= rollback_needed {
        return Err(Invalid::RollbackTimeout {
            rollback: settings.timeouts.rollback,
            wait: params.drain_wait_secs,
        });
    }
    if record.schema != SCHEMA {
        return Err(Invalid::Schema {
            found: record.schema,
        });
    }
    if update.unit != settings.unit {
        return Err(Invalid::Unit {
            found: update.unit.clone(),
        });
    }
    let versions = [
        ("target", Some(update.target.as_str())),
        ("previous", update.previous.as_deref()),
    ];
    for (field, version) in versions {
        if !version.is_none_or(is_version) {
            return Err(Invalid::Version { field });
        }
    }
    if update.previous.as_ref() == Some(&update.target) {
        return Err(Invalid::SameVersion);
    }
    if !is_digest(&update.artifact_digest) {
        return Err(Invalid::Digest);
    }
    let state_dir = &settings.state_dir;
    release_path("staged", &update.staged, state_dir)?;
    if let Some(previous) = &update.previous_target {
        release_path("previous_target", previous, state_dir)?;
    }
    if update.previous.is_some() != update.previous_target.is_some() {
        return Err(Invalid::Unpaired { side: "record" });
    }
    if params.previous_version.is_some() != params.previous_release.is_some() {
        return Err(Invalid::Unpaired { side: "params" });
    }
    if update.previous_target.as_deref() == Some(update.staged.as_path()) {
        return Err(Invalid::SameRelease);
    }
    // The updater's table is the order; the record must say what the order said.
    agrees(
        "target",
        Some(OsStr::new(&update.target)),
        Some(OsStr::new(&params.target_version)),
    )?;
    agrees(
        "previous",
        update.previous.as_deref().map(OsStr::new),
        params.previous_version.as_deref().map(OsStr::new),
    )?;
    agrees(
        "artifact_digest",
        Some(OsStr::new(&update.artifact_digest)),
        Some(OsStr::new(&params.artifact_digest)),
    )?;
    agrees(
        "staged",
        Some(update.staged.as_os_str()),
        Some(params.staged_release.as_os_str()),
    )?;
    agrees(
        "previous_target",
        update.previous_target.as_deref().map(Path::as_os_str),
        params.previous_release.as_deref().map(Path::as_os_str),
    )?;
    cordon(record, params)?;
    unit_restart(record)?;
    marks(record)
}

/// A restart plan names at least one change, and every name is printable text of a sensible
/// size: it goes into the report's detail.
fn unit_restart(record: &Record) -> Result<(), Invalid> {
    let Some(plan) = &record.unit_restart else {
        return Ok(());
    };
    let plain = |name: &String| {
        (1..=MAX_CHANGE_LEN).contains(&name.len()) && !name.chars().any(char::is_control)
    };
    if (1..=MAX_CHANGES).contains(&plan.changed.len()) && plan.changed.iter().all(plain) {
        Ok(())
    } else {
        Err(Invalid::UnitRestart)
    }
}

/// The cordon's stamps against its mark and the declared cap: the expiry is whole seconds, so
/// it may not exceed `first_ms + cordon_max_secs * 1000 + EXPIRY_SLACK_MS` (one second).
fn cordon(record: &Record, params: &Params) -> Result<(), Invalid> {
    let cordon = &record.cordon;
    if !cordon.placed {
        return if (cordon.first_ms, cordon.expires_ms) == (0, 0) && !cordon.provisional {
            Ok(())
        } else {
            Err(Invalid::CordonStamps)
        };
    }
    if cordon.first_ms == 0 || cordon.expires_ms < cordon.first_ms {
        return Err(Invalid::CordonStamps);
    }
    let limit_ms = cordon
        .first_ms
        .saturating_add(params.cordon_max_secs.saturating_mul(1000))
        .saturating_add(EXPIRY_SLACK_MS);
    if cordon.expires_ms > limit_ms {
        return Err(Invalid::CordonExpiry {
            expires_ms: cordon.expires_ms,
            limit_ms,
        });
    }
    Ok(())
}

/// Each mark is written before the act it names, and an act follows the one before it: a mark
/// set without the one it follows from cannot have been written by this hook.
fn marks(record: &Record) -> Result<(), Invalid> {
    let rb = &record.rb;
    let rules = [
        (
            "link_moved",
            record.link_moved,
            "stop_begun",
            // A first install has no unit to stop: the link move is its first act.
            record.stop_begun || record.update.previous.is_none(),
        ),
        (
            "start_issued",
            record.start_issued,
            "link_moved",
            record.link_moved,
        ),
        (
            "verify_ok",
            record.verify_ok,
            "start_issued",
            record.start_issued,
        ),
        ("rb.drained", rb.drained, "stop_begun", record.stop_begun),
        ("rb.stop_begun", rb.stop_begun, "rb.drained", rb.drained),
        (
            "rb.link_restored",
            rb.link_restored,
            "stop_begun",
            record.stop_begun,
        ),
        (
            "rb.start_issued",
            rb.start_issued,
            "rb.link_restored",
            rb.link_restored,
        ),
        (
            "rb.hello_ok",
            rb.hello_ok,
            "rb.start_issued",
            rb.start_issued,
        ),
    ];
    if let Some(&(mark, _, needs, _)) = rules.iter().find(|(_, set, _, needed)| *set && !*needed) {
        return Err(Invalid::Marks { mark, needs });
    }
    // The session before the stop is written with the stop mark; a first install has no stop.
    match (
        record.pre_stop_session_ms,
        record.stop_begun,
        record.update.previous.is_some(),
    ) {
        (Some(_), false, _) => {
            return Err(Invalid::Marks {
                mark: "pre_stop_session_ms",
                needs: "stop_begun",
            });
        }
        (Some(_), true, false) => {
            return Err(Invalid::Marks {
                mark: "pre_stop_session_ms",
                needs: "a previous release",
            });
        }
        _ => {}
    }
    // The session before the rollback's first act is written with the first of its act marks.
    let acted = rb.stop_begun || rb.link_restored || rb.start_issued;
    match (
        record.rb_pre_stop_session_ms,
        acted,
        record.update.previous.is_some(),
    ) {
        (Some(_), false, _) => Err(Invalid::Marks {
            mark: "rb_pre_stop_session_ms",
            needs: "an rb act mark",
        }),
        (Some(_), true, false) => Err(Invalid::Marks {
            mark: "rb_pre_stop_session_ms",
            needs: "a previous release",
        }),
        _ => Ok(()),
    }
}
