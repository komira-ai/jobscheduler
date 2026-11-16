//! The hook record: what a step did, kept where the next step (or a rerun of the same one)
//! finds it.
//!
//! The record is `state_dir/record.json`, kept through js-update-hook's
//! [`Held`](js_update_hook::Held): the lock, the checks of the directory and of every directory
//! above it, and the save that is the old record or the new one and never a part of either are
//! its. This module holds what is the config hook's: the record and the rule it is held to.
//! Each mark is written before the act it names, except `restarted` and `verified`, which are
//! written after the observation they name.

use js_update_hook::Document;
use serde::{Deserialize, Serialize};

use super::Params;
use super::reason::Reason;

/// The schema this hook writes and reads.
pub const SCHEMA: u32 = 1;

/// The hook record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The format of this file; see [`SCHEMA`].
    pub schema: u32,
    /// The unit the hook restarts.
    pub unit: String,
    /// `cutover` has read the host's session off the lease server, so that a rerun does not read
    /// the session of the agent it restarted.
    pub session_read: bool,
    /// The `session_started_ms` the server showed for the host before the restart; `None` when
    /// the server could not be asked, did not know the host or showed no session.
    pub session_started_ms: Option<u64>,
    /// The stop of the unit is begun: written before `cutover` stops it.
    pub restart_begun: bool,
    /// `cutover` restarted the unit and saw it active.
    pub restarted: bool,
    /// A `verify` succeeded.
    pub verified: bool,
}

impl Record {
    /// The record of a step that has done nothing yet, for the unit `params` name.
    #[must_use]
    pub fn new(params: &Params) -> Record {
        Record {
            schema: SCHEMA,
            unit: params.unit.clone(),
            session_read: false,
            session_started_ms: None,
            restart_begun: false,
            restarted: false,
            verified: false,
        }
    }
}

/// A rule of this hook that a record breaks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// The record's unit is not the one the params name.
    #[error("the record's unit {0:?} is not the unit the params name")]
    Unit(String),
}

impl Document for Record {
    const SCHEMA: u32 = SCHEMA;
    type Invalid = Invalid;
    type Reason = Reason;
    const LOCKED: Reason = Reason::Locked;
    const STATE_INVALID: Reason = Reason::StateInvalid;
}

/// The state directory, checked and locked, with the config hook's [`Record`] in it. The lock
/// is released when this is dropped.
pub type Held = js_update_hook::Held<Record>;

/// Why the state could not be used: `locked` for a second holder, `state_invalid` for
/// everything else.
pub type StateError = js_update_hook::StateError<Record>;
