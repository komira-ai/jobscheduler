//! A host's presence as a row (`node/<host>`, [`NodeRecord`]): its session (the epoch of its
//! last hello and the digest of that hello's secret), what the hello announced, and what its
//! last applied beat left (its phase, its load, its listing flags and the time the store's
//! clock read when the beat was applied). Every server that shares the store reads the same
//! row, so a beat that reaches any of them is judged and applied alike
//! ([`crate::JobStore::hello_node`], [`crate::JobStore::beat_node`]).

use std::collections::BTreeMap;
use std::fmt;

use js_agent_wire::Phase;
use js_state::{StoreTime, Version};
use js_types::{HostId, Shape};

use crate::codec::Carried;
use crate::ids::JobId;

/// The digest of the secret a host's hello was given, so a beat's secret is checked against
/// the row and the row holds no secret. Its `Debug` shows no bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionDigest([u8; 32]);

impl SessionDigest {
    /// The digest `bytes`.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> SessionDigest {
        SessionDigest(bytes)
    }

    /// The digest's bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SessionDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionDigest(..)")
    }
}

/// The scalar part of a load a host reported in a beat, in the units of
/// `js_agent_wire::NodeLoad`: what a placement reads of a host's load. The per-lease parts of
/// the wire's load (the leases covered, their clocks and closed minutes) are usage, which the
/// usage rows keep, and are not in the row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadReading {
    /// The reading's sequence number.
    pub seq: u64,
    /// The agent's monotonic clock at the reading, in microseconds.
    pub mono_usec: u64,
    /// Cumulative CPU time of the cgroup that holds every action, in microseconds.
    pub cpu_usage_usec: u64,
    /// Memory in use by the actions, in bytes.
    pub working_set_bytes: u64,
    /// Active file cache, in bytes.
    pub active_file_bytes: u64,
    /// Whether the actions cgroup has a CPU limit.
    pub actions_cpu_limited: bool,
}

/// What a beat writes into a host's row, decided by its caller on the row as the beat read
/// it ([`crate::JobStore::beat_node`]): the host's phase and load, its listing flags, and the
/// count of applied beats in a row that left out each confirmed container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    /// The phase the host last announced.
    pub phase: Phase,
    /// The load of the last applied beat that carried one; `None` after a hello, or when the
    /// beat carried none.
    pub load: Option<LoadReading>,
    /// Whether the first job listing since the hello is still to be applied: a hello sets it,
    /// and the beat that applies that listing clears it in its own write.
    pub first_listing_pending: bool,
    /// Whether the host's last job listing named only jobs it holds a live allocation for; a
    /// host whose last listing was not clean takes no job.
    pub last_listing_clean: bool,
    /// For a host whose agent sends no beat sequence: how many applied beats in a row left out
    /// each job whose container the host had confirmed, by job. Its container is freed only
    /// once two applied beats in a row leave it out.
    pub missing_in_a_row: BTreeMap<JobId, u32>,
}

/// What a hello announces, which [`crate::JobStore::hello_node`] writes into the host's row
/// with a new epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHello {
    /// The digest of the secret the hello is given; `None` for a host that is given none.
    pub session: Option<SessionDigest>,
    /// The pool the host joined.
    pub pool: String,
    /// The shape the host announced, with the capabilities it reported; `None` when its
    /// announced shape is not one.
    pub claim: Option<Shape>,
    /// The shape the pool books for the host.
    pub shape: Option<Shape>,
    /// The labels the host announced.
    pub labels: BTreeMap<String, String>,
    /// The phase the host announced.
    pub phase: Phase,
}

/// The epoch and sequence of the last beat a row applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedBeat {
    /// The epoch of the beat.
    pub epoch: u64,
    /// The highest sequence of the beats applied in that epoch; `None` when none of them
    /// carried one (an agent that sends no sequence).
    pub seq: Option<u64>,
}

/// The row `node/<host>`: a host's session, what its hello announced and what its beats left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// The host.
    pub host: HostId,
    /// The epoch of the host's last hello: one more than the row's before it, 1 for a host's
    /// first.
    pub epoch: u64,
    /// The digest of the last hello's secret.
    pub session: Option<SessionDigest>,
    /// The pool the host joined.
    pub pool: String,
    /// The shape the host announced, with the capabilities it reported.
    pub claim: Option<Shape>,
    /// The shape the pool books for the host.
    pub shape: Option<Shape>,
    /// The labels the host announced.
    pub labels: BTreeMap<String, String>,
    /// What the last hello or applied beat left.
    pub state: NodeState,
    /// The last beat applied in any epoch; `None` after a hello, until a beat is applied.
    pub last_beat: Option<AppliedBeat>,
    /// The store's clock when the last hello or applied beat was written.
    pub heard_at: StoreTime,
    /// Fields of a later build, written back unchanged.
    pub(crate) carried: Carried,
}

/// A host's row as read, with the version to write it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    /// The record.
    pub record: NodeRecord,
    /// The row's version: what a write conditional on the row is checked against.
    pub version: Version,
}

/// How [`crate::JobStore::beat_node`] judged a beat. Every answer but [`Beat::Applied`] wrote
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Beat {
    /// The beat was applied: the row as written, with the version a listing's writes are made
    /// conditional on.
    Applied(NodeRow),
    /// A beat of the row's epoch with a sequence no newer than the one applied: the row as it
    /// stands, which the caller answers from.
    Superseded(NodeRow),
    /// A beat of an epoch older than the row's: a later hello passed it.
    StaleEpoch(NodeRow),
    /// No hello of the beat's epoch is recorded: there is no row (`None`), or the row's epoch
    /// is older than the beat's. The host says hello.
    NoSession(Option<NodeRow>),
}

#[cfg(test)]
mod tests {
    use super::SessionDigest;

    // Catches a digest whose bytes a log of the record would show.
    #[test]
    fn a_session_digest_shows_no_bytes() {
        let digest = SessionDigest::new([0xab; 32]);
        assert_eq!(format!("{digest:?}"), "SessionDigest(..)");
        assert_eq!(digest.as_bytes(), &[0xab; 32]);
    }
}
