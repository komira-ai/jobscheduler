//! Which hosts the core hears from.
//!
//! An [`Admission`] is asked before the core takes a host's registration or heartbeat; a
//! host it refuses never reaches the scheduler. The core asks outside its lock, so a policy
//! that reads a list that changes (a deny list, a certificate store) does not stall the
//! farm.

use js_types::sched::WorkerId;

use crate::host::{Beat, Reported};

/// Why a host is not heard.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct Denied {
    /// What an operator reads.
    pub reason: String,
}

impl Denied {
    /// A refusal for `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// The policy that decides which hosts the core hears from.
pub trait Admission: Send + Sync {
    /// Whether `worker` may register, or report again, as `reported` says.
    ///
    /// # Errors
    /// Why not: the host's stream is ended with it.
    fn allow_host(&self, worker: &WorkerId, reported: &Reported) -> Result<(), Denied>;

    /// Whether `worker` may send `beat`.
    ///
    /// # Errors
    /// Why not.
    fn allow_beat(&self, worker: &WorkerId, beat: &Beat) -> Result<(), Denied>;
}

/// Hears every host.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl Admission for AllowAll {
    fn allow_host(&self, _: &WorkerId, _: &Reported) -> Result<(), Denied> {
        Ok(())
    }

    fn allow_beat(&self, _: &WorkerId, _: &Beat) -> Result<(), Denied> {
        Ok(())
    }
}
