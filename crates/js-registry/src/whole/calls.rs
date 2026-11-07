//! How many times the registry's reservation machine was called: the wait list
//! ([`super::queue`]), the request records ([`super::requests`]), the marks and the pass
//! ([`super::reserve`], `Registry::place_waiting`). With the `test-support` feature each
//! of them counts its calls, which [`crate::Registry::machine_calls`] reads, so a test can
//! show that a placer that keeps the wait list ([`crate::Placer::keeps_wait_list`]) leaves
//! the machine uncalled, and that the registry's own placement calls it. Without the
//! feature a count is nothing and costs nothing.

#[cfg(feature = "test-support")]
use std::cell::Cell;

/// The calls one part of the machine has had. The parts sit under the registry's mutex,
/// so a plain cell is enough.
#[derive(Debug, Default)]
pub(crate) struct Calls {
    #[cfg(feature = "test-support")]
    count: Cell<u64>,
}

impl Calls {
    /// Counts one call.
    pub(crate) fn note(&self) {
        #[cfg(feature = "test-support")]
        self.count.set(self.count.get().saturating_add(1));
    }

    /// The calls counted so far.
    #[cfg(feature = "test-support")]
    pub(crate) fn count(&self) -> u64 {
        self.count.get()
    }
}

/// The calls each part of the registry's reservation machine has had since the registry
/// was made ([`crate::Registry::machine_calls`]).
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MachineCalls {
    /// Calls of the wait list.
    pub wait_list: u64,
    /// Calls of the request records.
    pub records: u64,
    /// Calls of the marks.
    pub marks: u64,
    /// Passes of the registry's own: `Registry::place_waiting` and the reservation pass.
    pub passes: u64,
}

#[cfg(feature = "test-support")]
impl MachineCalls {
    /// Whether no part was called.
    #[must_use]
    pub fn none(&self) -> bool {
        *self == Self::default()
    }
}
