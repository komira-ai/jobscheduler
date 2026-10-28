//! The keys of the scheduler's rows in the state store, byte for byte as the farm server
//! writes them: a store written by one reads under the other, so a key never changes.
//!
//! An operation's rows are keyed by its id as sixteen lowercase hexadecimal digits, a lease's
//! by its term and sequence as two such numbers joined by `.`, a node's by the node's name as
//! it is, and the rows of a piece of work (a memory floor, an in-flight pin) by the work key's
//! class in hexadecimal bytes (the class may hold any character, a `/` too), `/`, and its
//! dedup text.

use std::fmt::Write as _;

use js_types::sched::{LeaseId, OperationId, WorkKey, WorkerId};

use crate::StateError;

/// `op/<operation>`: an unfinished operation's request.
pub const OPERATION: &str = "op/";
/// `queue/<operation>`: present while the operation waits for a lease.
pub const QUEUE: &str = "queue/";
/// `lease/<term>.<seq>`: the fencing token of a committed grant.
pub const LEASE: &str = "lease/";
/// `lease-by-op/<operation>`: the lease an operation holds.
pub const LEASE_BY_OP: &str = "lease-by-op/";
/// `session/<worker>`: a daemon's newest session, expiring unless renewed.
pub const SESSION: &str = "session/";
/// `owner/<worker>`: the frontend that owns a node.
pub const OWNER: &str = "owner/";
/// `cordon/<worker>`: how an operator placed a node.
pub const CORDON: &str = "cordon/";
/// `done/<operation>`: how a finished or refused operation ended.
pub const DONE: &str = "done/";
/// `loss/<operation>`: how many leases of an unfinished operation were lost.
pub const LOSS: &str = "loss/";
/// `fate/<operation>`: what an operation's faults changed.
pub const FATE: &str = "fate/";
/// `floor/<class>/<dedup>`: a memory floor.
pub const FLOOR: &str = "floor/";
/// `keyfloor/<key>`: a key floor.
pub const KEY_FLOOR: &str = "keyfloor/";
/// `opkey/<operation>`: the size key of an operation.
pub const OPKEY: &str = "opkey/";
/// `inflight/<class>/<dedup>`: which operation runs a piece of work now.
pub const INFLIGHT: &str = "inflight/";

/// `meta/epoch`: the store incarnation, eight bytes, big-endian.
pub const EPOCH: &str = "meta/epoch";
/// `meta/alive`: written to tell whether the store takes writes again; empty.
pub const ALIVE: &str = "meta/alive";
/// `meta/next_op`: the first operation id no frontend has reserved, eight bytes, big-endian.
pub const NEXT_OP: &str = "meta/next_op";
/// `meta/next_seq`: the first lease sequence no frontend has reserved, eight bytes,
/// big-endian.
pub const NEXT_SEQ: &str = "meta/next_seq";
// `meta/generation` is `js_state::GENERATION`: the state store's own row.

/// Every prefix of a row this crate owns, and every `meta/` key: what an embedder's rows
/// ([`crate::Ext`]) must stay out of. A kind of row added above is added here too.
pub const ALL: [&str; 18] = [
    OPERATION,
    QUEUE,
    LEASE,
    LEASE_BY_OP,
    SESSION,
    OWNER,
    CORDON,
    DONE,
    LOSS,
    FATE,
    FLOOR,
    KEY_FLOOR,
    OPKEY,
    INFLIGHT,
    EPOCH,
    ALIVE,
    NEXT_OP,
    NEXT_SEQ,
];

/// `prefix` and the operation's id as sixteen hexadecimal digits.
fn of_operation(prefix: &str, operation: OperationId) -> String {
    format!("{prefix}{:016x}", operation.0)
}

/// `prefix`, the class of `key` as hexadecimal bytes, `/`, and its dedup text.
fn of_work(prefix: &str, key: &WorkKey) -> String {
    let mut class = String::with_capacity(2 * key.class.len());
    for byte in key.class.bytes() {
        // Writing to a String does not fail.
        let _ = write!(class, "{byte:02x}");
    }
    format!("{prefix}{class}/{}", key.dedup)
}

/// The `op/` row of `operation`.
#[must_use]
pub fn operation(operation: OperationId) -> String {
    of_operation(OPERATION, operation)
}

/// The `queue/` row of `operation`.
#[must_use]
pub fn queue(operation: OperationId) -> String {
    of_operation(QUEUE, operation)
}

/// The `loss/` row of `operation`.
#[must_use]
pub fn loss(operation: OperationId) -> String {
    of_operation(LOSS, operation)
}

/// The `fate/` row of `operation`.
#[must_use]
pub fn fate(operation: OperationId) -> String {
    of_operation(FATE, operation)
}

/// The `opkey/` row of `operation`.
#[must_use]
pub fn opkey(operation: OperationId) -> String {
    of_operation(OPKEY, operation)
}

/// The `done/` row of `operation`.
#[must_use]
pub fn done(operation: OperationId) -> String {
    of_operation(DONE, operation)
}

/// The `lease-by-op/` row of `operation`.
#[must_use]
pub fn lease_by_op(operation: OperationId) -> String {
    of_operation(LEASE_BY_OP, operation)
}

/// The `lease/` row of `lease`: its term and sequence as sixteen hexadecimal digits each.
#[must_use]
pub fn lease(lease: LeaseId) -> String {
    format!("{LEASE}{:016x}.{:016x}", lease.term, lease.seq)
}

/// The `session/` row of `worker`.
#[must_use]
pub fn session(worker: &WorkerId) -> String {
    format!("{SESSION}{worker}")
}

/// The `owner/` row of `worker`.
#[must_use]
pub fn owner(worker: &WorkerId) -> String {
    format!("{OWNER}{worker}")
}

/// The `cordon/` row of `worker`.
#[must_use]
pub fn cordon(worker: &WorkerId) -> String {
    format!("{CORDON}{worker}")
}

/// The `floor/` row of the memory floor of `key`.
#[must_use]
pub fn floor(key: &WorkKey) -> String {
    of_work(FLOOR, key)
}

/// The `inflight/` row of the work `key`.
#[must_use]
pub fn inflight(key: &WorkKey) -> String {
    of_work(INFLIGHT, key)
}

/// The `keyfloor/` row of size key `key`: sixteen hexadecimal digits.
#[must_use]
pub fn key_floor(key: u64) -> String {
    format!("{KEY_FLOOR}{key:016x}")
}

/// The operation a row key under `prefix` names.
///
/// # Errors
/// [`StateError::Corrupt`]: `key` does not start with `prefix`, or what follows is not
/// hexadecimal that fits an id.
pub fn operation_of(prefix: &str, key: &str) -> Result<OperationId, StateError> {
    key.strip_prefix(prefix)
        .and_then(|id| u64::from_str_radix(id, 16).ok())
        .map(OperationId)
        .ok_or_else(|| StateError::corrupt(key, "not an operation id"))
}

/// The node a row key under `prefix` names, or `None` if `key` is not under `prefix`.
#[must_use]
pub fn worker_of(prefix: &str, key: &str) -> Option<WorkerId> {
    key.strip_prefix(prefix).map(WorkerId::new)
}
