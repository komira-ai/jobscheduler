//! The hook record: what a step did, kept where the next step (or a rerun of the same one)
//! finds it.
//!
//! The record is `state_dir/record.json`, kept through js-update-hook's
//! [`Held`](js_update_hook::Held): the lock, the checks of the directory and of every directory
//! above it, and the save that is the old record or the new one and never a part of either are
//! its. This module holds what is the server's: the record, its schema, and the rules a record
//! is held to ([`validate`]). A record whose schema is newer than [`SCHEMA`] is read-only: it is
//! not loaded, and it is not overwritten.
//!
//! Each mark is written before the act it names, so a step that is killed leaves the mark of
//! every act it may have made; a rerun asks the machine whether the act happened.

use std::path::PathBuf;

use js_update_hook::Document;
use serde::{Deserialize, Serialize};

use super::declaration::Declaration;
use super::params::Params;
use super::report::Reason;

/// The schema this hook writes and reads.
pub const SCHEMA: u32 = 1;

/// The marks of `rollback`'s acts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollback {
    /// The stop of the staged release is begun.
    pub stop_begun: bool,
    /// The move of the link back to the previous release is begun.
    pub link_restored: bool,
    /// The start of the unit on the previous release is begun.
    pub start_issued: bool,
    /// The previous release was seen holding the writer lock.
    pub serving_ok: bool,
}

/// The hook record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The format of this file; see [`SCHEMA`].
    pub schema: u32,
    /// The version being installed.
    pub target: String,
    /// The version that runs now; `None` on a first install.
    pub previous: Option<String>,
    /// The staged release directory of `target`.
    pub staged: PathBuf,
    /// Where the release link pointed when `prepare` read it: the previous release, `None` on
    /// a first install (no link).
    pub previous_target: Option<PathBuf>,
    /// The unit that runs the server.
    pub unit: String,
    /// Who held the store's writer lock when `prepare` probed it; `None` when nobody did (the
    /// server was stopped). An opaque identifier: `verify` asks for another, by equality.
    pub holder: Option<String>,
    /// The schema version the store held when `prepare` read it; `None` when the store's
    /// backend has no reader of it that leaves the store alone (SQLite).
    pub store_schema: Option<u32>,
    /// The stop is begun: written before `cutover` stops the unit. The commit.
    pub stop_begun: bool,
    /// The move of the link to the staged release is begun.
    pub link_moved: bool,
    /// The start of the unit on the staged release is begun.
    pub start_issued: bool,
    /// A `verify` succeeded.
    pub verify_ok: bool,
    /// What `rollback` has done.
    pub rb: Rollback,
    /// Who held the writer lock when `rollback` probed it before its first act; `None` when
    /// nobody did, and until that act. Written in the same save as the first of `rb`'s marks;
    /// the previous release proves itself by holding the lock as someone else.
    pub rb_holder: Option<String>,
}

impl Record {
    /// The record `prepare` writes: what it saw, nothing done.
    #[must_use]
    pub fn new(params: &Params, previous_target: Option<PathBuf>) -> Record {
        Record {
            schema: SCHEMA,
            target: params.target_version.clone(),
            previous: params.previous_version.clone(),
            staged: params.staged_release.clone(),
            previous_target,
            unit: params.unit.clone(),
            holder: None,
            store_schema: None,
            stop_begun: false,
            link_moved: false,
            start_issued: false,
            verify_ok: false,
            rb: Rollback::default(),
            rb_holder: None,
        }
    }
}

/// A rule of this hook that a record breaks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// The record's unit is not the declared one, or not the one `--unit` names.
    #[error("the record's unit {0:?} is not the declared unit named by --unit")]
    Unit(String),
    /// The record is of another update: its staged release, target version or previous
    /// release is not the params table's.
    #[error("the record is of another update than the params table's")]
    OtherUpdate,
}

impl Document for Record {
    const SCHEMA: u32 = SCHEMA;
    type Invalid = Invalid;
    type Reason = Reason;
    const LOCKED: Reason = Reason::Locked;
    const STATE_INVALID: Reason = Reason::StateInvalid;
}

/// The state directory, checked and locked, with the server's [`Record`] in it. The lock is
/// released when this is dropped.
pub type Held = js_update_hook::Held<Record>;

/// Why the state could not be used: `locked` for a second holder, `state_invalid` for
/// everything else.
pub type StateError = js_update_hook::StateError<Record>;

/// Holds `record` to the declaration and the params table: the declared unit, the one
/// `--unit` names, and the update the table is of.
///
/// # Errors
/// The first [`Invalid`] rule the record breaks.
pub fn validate(
    record: &Record,
    declaration: &Declaration,
    params: &Params,
) -> Result<(), Invalid> {
    if record.unit != declaration.unit || record.unit != params.unit {
        return Err(Invalid::Unit(record.unit.clone()));
    }
    let same = record.staged == params.staged_release
        && record.target == params.target_version
        && record.previous_target == params.previous_release;
    match same {
        true => Ok(()),
        false => Err(Invalid::OtherUpdate),
    }
}
