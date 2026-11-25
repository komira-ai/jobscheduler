//! The operator's retirement of a host id ([`Agents::retire`], the job service's `RetireHost`):
//! a host that changed its id (a host-certificate join after a token join with another id), or
//! that is gone, leaves behind a row, a binding and a record that each keep counting toward its
//! pool's `max_machines`, so a fixed pool sized to its machines refuses the new id `pool-full`.
//! A retirement deletes all three, once the server has judged that the host is not live and
//! holds nothing: no job's allocation, no taint, no cordon and no scale-down mark.
//!
//! The judgement is the registry's and the stored binding's first (a host this server holds
//! that is live, tainted, cordoned or marked is kept, and so is one whose stored binding holds
//! a taint, so that no server's retirement ends a taint another server stored), then the job
//! store's, which deletes the row at the version it
//! read ([`js_jobs::JobStore::retire_node`]): a hello or beat that lands in between keeps it.
//! Only after the row is gone are the registry's record and the stored binding forgotten, so a
//! refusal changes nothing. Every step is under [`Agents::host_writes`], as a hello and a
//! cordon are. A step that fails after the row is gone leaves the rest to the same call
//! again, which finds no row and forgets what is left: a repeat is the retry, and a retirement
//! with nothing left to forget answers `false` and writes nothing. Another server over the same
//! store keeps its own record of the host until it forgets it as lost, or until the same call
//! is made on it.

use std::fmt::Display;
use std::sync::Arc;

use js_jobs::{Kept, Retire};
use js_registry::{HostState, OwnHost};
use js_types::HostId;
use tracing::{error, info};

use super::Agents;
use crate::util::blocking;

/// Why a retirement was not made, or not wholly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetireFault {
    /// The host is kept, for the reason given: nothing was written.
    Kept(Kept),
    /// The job store or the bindings could not be read or written. What was deleted before
    /// stays deleted, and the same call again does the rest.
    Unsaved,
}

impl Agents {
    /// Retires `host`: deletes its `node/` row at the version read, forgets this server's
    /// record of it and deletes its stored binding (in file mode, saves the file without it), so
    /// that neither the pool's count over the rows nor this server's count holds it any more.
    /// Answers whether anything of the host was deleted; `false`, with nothing written, for a
    /// host with nothing left to retire.
    ///
    /// # Errors
    ///
    /// [`RetireFault::Kept`] for a host this server holds as live, tainted, cordoned or marked,
    /// whose stored binding holds a taint (also when this server's registry never held the
    /// host), or whose row the job store keeps ([`js_jobs::Kept`]); [`RetireFault::Unsaved`]
    /// when a store could not be read or written.
    pub(crate) async fn retire(self: &Arc<Self>, host: &HostId) -> Result<bool, RetireFault> {
        let gate = self.host_writes.lock().await;
        // What the store holds of the host is taken into the registry first, as before a hello:
        // a taint another server stored keeps the host (and is not written over by the next
        // save), and the binding's version is the one its delete below is made at.
        self.restore_stored(host, "retire").await.map_err(unsaved)?;
        let own = self.registry.own_host(host);
        if let Some(kept) = own.as_ref().and_then(held_by) {
            return Err(RetireFault::Kept(kept));
        }
        let judged = match self.jobs.clone() {
            Some(jobs) => {
                let (id, grace) = (host.clone(), self.registry.longest_grace());
                let judged = blocking(move || jobs.retire_node(&id, grace)).await;
                judged.map_err(unsaved)?
            }
            None => Retire::Absent,
        };
        let row = match judged {
            Retire::Refused(kept) => return Err(RetireFault::Kept(kept)),
            Retire::Retired => true,
            Retire::Absent => false,
        };
        let record = self.registry.forget_host(host);
        let (this, id) = (Arc::clone(self), host.clone());
        let binding = blocking(move || this.store.forget(&id)).await;
        let binding = binding.map_err(unsaved)?;
        drop(gate);
        // A save that fails is logged by the save itself.
        self.saved().await.map_err(|_| RetireFault::Unsaved)?;
        info!(%host, row, record, binding, "a host was retired");
        Ok(row | record | binding)
    }
}

/// What this server's record of a host says keeps it: live first, then a taint, a cordon and
/// a scale-down mark; `None` when it says none of these.
fn held_by(own: &OwnHost) -> Option<Kept> {
    let host = &own.host;
    let reasons = [
        (host.state == HostState::Live, Kept::Live),
        (host.taint.is_some(), Kept::Tainted),
        (own.cordon_until.is_some(), Kept::Cordoned),
        (host.mark.is_some(), Kept::Marked),
    ];
    reasons
        .into_iter()
        .find_map(|(holds, kept)| holds.then_some(kept))
}

/// The fault of a retirement whose store failed, logged as an error naming the failure.
fn unsaved(error: impl Display) -> RetireFault {
    error!(%error, "cannot retire a host: a store could not be read or written");
    RetireFault::Unsaved
}
