//! The encoding of the values of the `op/`, `lease/` and `session/` rows ([`RowCodec`]).
//!
//! This crate decides which rows exist, what their keys are and when each is written; the
//! codec decides only what bytes a request, a lease grant and a daemon's registration are as
//! a row value. The server that embeds the crate supplies one (the farm server's writes the
//! encoding its replicated log already runs), so what a row may hold is that server's to
//! version, and rows only ever gain new kinds. `test_support::TestCodec`, with the
//! `test-support` feature, is the codec the tests of this crate and its users run on.

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Request};
use js_types::sched::{LeaseGrant, Resources, WorkerId};

use crate::StateError;

/// What a daemon's registration says, as its `session/` row keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerUp {
    /// The worker.
    pub worker: WorkerId,
    /// The daemon process that registered it.
    pub instance: DaemonInstance,
    /// The server found the daemon process before it stopped, its runs killed.
    pub predecessor_stopped: bool,
    /// What placement may book on it.
    pub capacity: Resources,
    /// What it offers.
    pub caps: NodeCaps,
}

/// What a row value holds, as [`RowCodec::read`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A request (`op/`).
    Submit(Request),
    /// A lease grant (`lease/`).
    Lease(LeaseGrant),
    /// A daemon's registration (`session/`).
    Session(WorkerUp),
    /// A value the codec reads, but of a kind no row holds.
    Other,
    /// A value a later binary wrote: this one cannot read it. A scan skips such a row and
    /// logs it at a start; a row a write depends on is refused.
    Unknown,
}

/// The bytes of the values of the `op/`, `lease/` and `session/` rows, and the reading of
/// them back.
pub trait RowCodec: Send + Sync {
    /// The value of the `op/` row of `request`.
    ///
    /// # Errors
    /// The request cannot be encoded ([`StateError::Codec`]).
    fn submit(&self, request: &Request) -> Result<Vec<u8>, StateError>;

    /// The value of the `lease/` row of `grant`.
    ///
    /// # Errors
    /// The grant cannot be encoded ([`StateError::Codec`]).
    fn lease(&self, grant: &LeaseGrant) -> Result<Vec<u8>, StateError>;

    /// The value of the `session/` row of a registration.
    ///
    /// # Errors
    /// The registration cannot be encoded ([`StateError::Codec`]).
    fn session(&self, up: &WorkerUp) -> Result<Vec<u8>, StateError>;

    /// What the row `key` holds in `bytes`. A value a later binary wrote is
    /// [`Value::Unknown`], not an error.
    ///
    /// # Errors
    /// [`StateError::Corrupt`], naming `key`, if the bytes are not a value of this codec.
    fn read(&self, key: &str, bytes: &[u8]) -> Result<Value, StateError>;
}
