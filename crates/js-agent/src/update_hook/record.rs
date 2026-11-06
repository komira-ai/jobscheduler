//! The hook record: what a step did, kept where the next step (or a rerun of the same one)
//! finds it (host-updates design, 4.2, "The hook record").
//!
//! The record is `state_dir/record.json`, kept through js-update-hook's [`Held`]: the lock, the
//! checks of the directory and of every directory above it, and the save that is the old record
//! or the new one and never a part of either are its. This module holds what is the agent's:
//! the record itself, its schema, and the reasons and rules a [`StateError`] is reported and
//! checked with. A record whose schema is newer than [`SCHEMA`] is read-only: it is not loaded,
//! and it is not overwritten.

use std::path::PathBuf;

use js_update_hook::Document;
pub use js_update_hook::{
    DirProblem, DirStat, LOCK_FILE, RECORD_FILE, RECORD_MAX, TEMP_FILE, check_ancestor, check_dir,
    fd_stat,
};
use serde::{Deserialize, Serialize};

use super::report::Reason;
use super::validate::Invalid;

/// The schema this hook writes and reads.
pub const SCHEMA: u32 = 1;

/// Which update the record is of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateInfo {
    /// The version being installed.
    pub target: String,
    /// The version that runs now; `None` on a first install.
    pub previous: Option<String>,
    /// The staged release directory of `target`; the table's `updater.staged_release`.
    pub staged: PathBuf,
    /// The release directory the link pointed to before the update; `None` on a first install.
    pub previous_target: Option<PathBuf>,
    /// The unit that runs the agent.
    pub unit: String,
    /// The digest of the artifact the staged release was made from, `sha256:<hex>`.
    pub artifact_digest: String,
}

/// The cordon this update placed on the host, if it did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cordon {
    /// Whether this update placed it. A cordon that was there before is not the hook's to lift.
    pub placed: bool,
    /// The mark was written before the cordon was asked of the lease server and no reply has
    /// confirmed it: a cordon found in force is then not known to be ours. Cleared by a reply
    /// that says the host was not cordoned.
    #[serde(default)]
    pub provisional: bool,
    /// When it was first placed, in milliseconds on the wall clock; 0 when not placed.
    pub first_ms: u64,
    /// When it now expires, in whole seconds' worth of milliseconds: never later than
    /// `first_ms + cordon_max_secs * 1000` plus the one second the validator allows
    /// ([`EXPIRY_SLACK_MS`](super::validate::EXPIRY_SLACK_MS)). 0 when not placed.
    pub expires_ms: u64,
}

/// The marks of `rollback`'s sub-steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollback {
    /// The bounded wait saw no lease.
    pub drained: bool,
    /// The stop was begun (written before the signal).
    pub stop_begun: bool,
    /// The link was moved back to the previous release.
    pub link_restored: bool,
    /// The unit was started again.
    pub start_issued: bool,
    /// The previous version's hello was seen.
    pub hello_ok: bool,
}

/// The hook record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The format of this file; see [`SCHEMA`].
    pub schema: u32,
    /// Which update.
    pub update: UpdateInfo,
    /// The cordon.
    pub cordon: Cordon,
    /// The stop is begun: written and synced before `cutover` signals the unit, so a
    /// `cutover` killed at any later point leaves it set.
    pub stop_begun: bool,
    /// The `session_started_ms` the lease server showed for the host in the reading `cutover`
    /// took last before it marked the stop, on the server's clock; `None` on a first install
    /// and when the server showed no session. Written in the same save as `stop_begun`, never
    /// overwritten by a rerun, and the only thing `verify` compares a new session with: two
    /// stamps of the one clock, never one clock with another. Left out of the file when
    /// `None`, so such a record reads as it did before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_stop_session_ms: Option<u64>,
    /// The move of the link to the staged release is begun: written before the move, so a
    /// `cutover` killed at any later point leaves it set. The link itself says whether it
    /// happened.
    pub link_moved: bool,
    /// The start of the unit on the staged release is begun: written before the start. The
    /// unit manager says whether it happened.
    pub start_issued: bool,
    /// A `verify` succeeded.
    pub verify_ok: bool,
    /// What `rollback` has done.
    pub rb: Rollback,
    /// The `session_started_ms` the lease server showed for the host in the reading `rollback`
    /// took last before its first act on the unit or the link, on the server's clock; `None`
    /// when the server showed no session and until that first act. Written in the same save as
    /// the first of the marks `rb.stop_begun`, `rb.link_restored` and `rb.start_issued`, never
    /// overwritten by a rerun, and what the previous agent's hello is compared with by
    /// equality (never against the host's clock). Left out of the file when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rb_pre_stop_session_ms: Option<u64>,
}

impl Record {
    /// The record of a `prepare` that has just begun: nothing done.
    #[must_use]
    pub fn new(update: UpdateInfo) -> Record {
        Record {
            schema: SCHEMA,
            update,
            cordon: Cordon::default(),
            stop_begun: false,
            pre_stop_session_ms: None,
            link_moved: false,
            start_issued: false,
            verify_ok: false,
            rb: Rollback::default(),
            rb_pre_stop_session_ms: None,
        }
    }
}

impl Document for Record {
    const SCHEMA: u32 = SCHEMA;
    type Invalid = Invalid;
    type Reason = Reason;
    const LOCKED: Reason = Reason::Locked;
    const STATE_INVALID: Reason = Reason::StateInvalid;
}

/// The state directory, checked and locked, with the agent's [`Record`] in it (see
/// [`js_update_hook::Held`]). The lock is released when this is dropped.
pub type Held = js_update_hook::Held<Record>;

/// Why the state could not be used: `locked` for a second holder, `state_invalid` for
/// everything else, and [`Invalid`] for a record that breaks a rule of the declaration.
pub type StateError = js_update_hook::StateError<Record>;
