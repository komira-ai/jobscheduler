//! What the hook's steps touch, as ports: the unit manager, the release link, the store, the
//! server's `/healthz`, time and the kill points.
//!
//! Each port is a small trait with the machine's implementation (in
//! [`machine`](super::Machine)) and a fake beside the tests. The steps take a [`Ports`] and
//! never reach for the machine themselves. The unit manager, the release link, time and the
//! kill points are js-update-hook's, shared with every hook; the store and `/healthz` are the
//! server's own.

pub use js_update_hook::{Clock, Gate, Link, Units};

/// Whether the store's writer lock is held, and by whom, as a probe found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writer {
    /// No holder was found: no server runs on the store.
    Free,
    /// A holder has the lock.
    Held {
        /// Who: an opaque identifier, equal for the same session and different for another
        /// (js-state's `LockHolder`).
        holder: String,
    },
}

/// The store could not be reached, or did not answer: no more is said, so that nothing of the
/// connection (its address, its role, a password) can reach the hook's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the store could not be reached")]
pub struct StoreUnreachable;

/// What the store's schema says of this release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schema {
    /// A version this release runs on: at least its oldest. A version past its newest is one
    /// too: only the server's own start check can tell whether what lies above is an addition
    /// this release runs on, and `verify` sees a server that refused to start.
    Runs(u32),
    /// The store holds no schema.
    Missing,
    /// A version older than the oldest this release runs on.
    TooOld {
        /// The store's.
        found: u32,
        /// The oldest this release runs on.
        min: u32,
    },
    /// The backend has no reader of the version that leaves the store alone (SQLite, whose
    /// version is read only by opening the file, which takes the writer lock).
    Unchecked,
}

/// The store the server keeps its state in: read only, and never through its writer lock.
pub trait StoreView {
    /// Whether the store's writer lock is held, and by whom, found without taking it.
    ///
    /// # Errors
    /// [`StoreUnreachable`].
    fn writer(&self) -> Result<Writer, StoreUnreachable>;

    /// The store's schema version, held to the range this release runs on.
    ///
    /// # Errors
    /// [`StoreUnreachable`].
    fn schema(&self) -> Result<Schema, StoreUnreachable>;
}

/// The server's `GET /healthz`.
pub trait Health {
    /// The status `GET /healthz` answered with.
    ///
    /// # Errors
    /// No answer: the reason, which names the address and nothing else.
    fn status(&self) -> Result<u16, String>;
}

/// Everything a step may touch.
pub struct Ports<'a> {
    /// The unit manager.
    pub units: &'a dyn Units,
    /// The release link.
    pub link: &'a dyn Link,
    /// The store.
    pub store: &'a dyn StoreView,
    /// The server's `/healthz`.
    pub health: &'a dyn Health,
    /// Time.
    pub clock: &'a dyn Clock,
    /// Kill points.
    pub gate: &'a dyn Gate,
}

impl Ports<'_> {
    /// A kill point: a no-op on the machine.
    pub fn point(&self) {
        self.gate.point();
    }
}
