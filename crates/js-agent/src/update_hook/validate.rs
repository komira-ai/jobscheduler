//! The rules every value read from the record is held to before it is used (host-updates
//! design, 4.2, "Every value read from the record is validated against the declaration").
//!
//! The record is the hook's own memory, kept where only root writes; the rules are for the
//! day that stops being true (a corrupt file, a record of another version, a directory that
//! was not what it should be). A record that fails one is not acted on: the step exits 1 with
//! the reason `state_invalid` and changes nothing. Where the record and the system disagree
//! about what was done, the system is believed; that is a step's decision and not a rule here.

use std::path::{Component, Path, PathBuf};

use super::declaration::Declaration;
use super::params::Params;
use super::record::{Record, SCHEMA};

/// The longest version string a record may hold.
pub const MAX_VERSION_LEN: usize = 128;

/// How far past `first_ms + cordon_max_secs` a recorded cordon expiry may lie, in
/// milliseconds: the server counts whole seconds, the hook's clock does not.
pub const EXPIRY_SLACK_MS: u64 = 1_000;

/// The rule a record broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// The record's schema is not the one this hook writes.
    #[error("schema {found} is not {SCHEMA}")]
    Schema {
        /// The record's.
        found: u32,
    },
    /// A release path is not absolute and plain, or not under a declared release directory.
    #[error(
        "{field} is not an absolute path of plain components under a declared release directory"
    )]
    Path {
        /// The record field.
        field: &'static str,
    },
    /// The record's unit is not one the declaration names.
    #[error("unit {found:?} is not a declared unit")]
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
    /// A mark is set without the one it follows from.
    #[error("{mark} is set but {needs} is not")]
    Marks {
        /// The mark that is set.
        mark: &'static str,
        /// The mark it needs.
        needs: &'static str,
    },
}

/// Whether `path` is spelt the way it would be after canonicalising it lexically: absolute,
/// no `.` or `..`, no doubled or trailing slash.
fn is_plain(path: &Path) -> bool {
    let rebuilt: PathBuf = path.components().collect();
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && rebuilt.as_os_str() == path.as_os_str()
}

/// A release path: plain, and strictly below one of the declared release directories.
fn release_path(
    field: &'static str,
    path: &Path,
    declaration: &Declaration,
) -> Result<(), Invalid> {
    let below = declaration
        .release_dirs()
        .any(|dir| path.starts_with(dir) && path != dir);
    if is_plain(path) && below {
        Ok(())
    } else {
        Err(Invalid::Path { field })
    }
}

/// A version string.
fn is_version(text: &str) -> bool {
    (1..=MAX_VERSION_LEN).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'~' | b'-'))
}

/// A digest as the registry writes it.
fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Runs every rule on `record`.
///
/// # Errors
/// The first [`Invalid`] rule the record breaks.
pub fn validate(
    record: &Record,
    declaration: &Declaration,
    params: &Params,
) -> Result<(), Invalid> {
    let update = &record.update;
    if record.schema != SCHEMA {
        return Err(Invalid::Schema {
            found: record.schema,
        });
    }
    if !declaration.units.contains(&update.unit) {
        return Err(Invalid::Unit {
            found: update.unit.clone(),
        });
    }
    for (field, version) in [("target", &update.target), ("previous", &update.previous)] {
        if !is_version(version) {
            return Err(Invalid::Version { field });
        }
    }
    if update.target == update.previous {
        return Err(Invalid::SameVersion);
    }
    if !is_digest(&update.artifact_digest) {
        return Err(Invalid::Digest);
    }
    release_path("staged", &update.staged, declaration)?;
    release_path("previous_target", &update.previous_target, declaration)?;
    cordon(record, params)?;
    marks(record)
}

/// The cordon's stamps against its mark and the declared cap.
fn cordon(record: &Record, params: &Params) -> Result<(), Invalid> {
    let cordon = &record.cordon;
    if !cordon.placed {
        return if (cordon.first_ms, cordon.expires_ms) == (0, 0) {
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

/// Each mark is written after the act it names, and an act follows the one before it: a mark
/// set without the one it follows from cannot have been written by this hook.
fn marks(record: &Record) -> Result<(), Invalid> {
    let rb = &record.rb;
    let rules = [
        (
            "link_moved",
            record.link_moved,
            "stop_begun",
            record.stop_begun,
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
    match rules.iter().find(|(_, set, _, needed)| *set && !*needed) {
        Some(&(mark, _, needs, _)) => Err(Invalid::Marks { mark, needs }),
        None => Ok(()),
    }
}
