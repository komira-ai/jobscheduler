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
use js_state::Versioned;
use js_types::sched::{LeaseGrant, Resources, WorkerId};

use crate::{Rows, StateError};

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

/// The error for the row `key`, which holds a command this binary does not know.
fn not_known(key: &str) -> StateError {
    StateError::corrupt(key, "a command this binary does not know")
}

/// Why `value`, which a row holds, is not the `what` the reader wants.
fn wrong(key: &str, value: &Value, what: &str) -> StateError {
    if matches!(value, Value::Unknown) {
        not_known(key)
    } else {
        StateError::corrupt(key, what)
    }
}

impl Rows {
    /// Whether `bytes`, the value of the row `key`, hold a command this binary does not
    /// know, which a later one wrote. A value that is malformed is not: a scan leaves it
    /// for the reader to refuse.
    pub(crate) fn is_unknown(&self, key: &str, bytes: &[u8]) -> bool {
        matches!(self.codec.read(key, bytes), Ok(Value::Unknown))
    }

    /// The grant a `lease/` row holds.
    pub(crate) fn lease_row(&self, key: &str, row: &Versioned) -> Result<LeaseGrant, StateError> {
        match self.codec.read(key, &row.value)? {
            Value::Lease(grant) => Ok(grant),
            other => Err(wrong(key, &other, "not a lease grant")),
        }
    }

    /// The request an `op/` row holds.
    pub(crate) fn request_row(&self, key: &str, row: &Versioned) -> Result<Request, StateError> {
        match self.codec.read(key, &row.value)? {
            Value::Submit(request) => Ok(request),
            other => Err(wrong(key, &other, "not a request")),
        }
    }

    /// The rows under `prefix` that a scan can use: a row that holds a command this binary
    /// does not know is left out, and every other row stays, for the scan to read or
    /// refuse. It logs nothing: a scan a request makes would repeat it per request.
    pub(crate) fn scan_known(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StateError> {
        let mut rows = self.store.scan_prefix(prefix)?;
        rows.retain(|(key, row)| !self.is_unknown(key, &row.value));
        Ok(rows)
    }

    /// [`Rows::scan_known`], logging the key of each row it leaves out: for a start.
    pub(crate) fn scan_known_logged(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, Versioned)>, StateError> {
        let mut rows = self.store.scan_prefix(prefix)?;
        rows.retain(|(key, row)| {
            let unknown = self.is_unknown(key, &row.value);
            if unknown {
                tracing::warn!(%key, "a row holds a command this binary does not know: skipped");
            }
            !unknown
        });
        Ok(rows)
    }
}
