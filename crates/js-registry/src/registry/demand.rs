//! What the registry tells a scaler about demand ([`Registry::waiting`]), and what a
//! scaler tells the registry about supply ([`Launchable`]).

use std::fmt;
use std::sync::Arc;

use js_elastic::Demand;
use js_lease::Timestamp;
use js_types::{HostId, RequestId};

use super::Registry;

/// One whole-node request on the wait list ([`Registry::waiting`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// The request id.
    pub id: RequestId,
    /// What the request needs of a host: its resources, and `os` and `arch` from the
    /// capabilities it requires.
    pub demand: Demand,
    /// When the request was first presented; how long it has waited counts from here.
    pub since: Timestamp,
    /// The host set aside for the request, free or busy, if it holds one. Such a request
    /// already has a host coming.
    pub reserved: Option<HostId>,
}

/// Whether a request that finds no host could be served by launching one. The shell that
/// owns the elastic pool supplies it ([`Registry::set_launchable`]).
///
/// The registry calls it with its lock held, so it must be quick, and it must not call
/// the registry.
#[derive(Clone)]
pub struct Launchable(Arc<dyn Fn(&Demand) -> bool + Send + Sync>);

impl Launchable {
    /// A `Launchable` that answers with `could_launch`.
    pub fn new(could_launch: impl Fn(&Demand) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(could_launch))
    }

    pub(super) fn allows(&self, demand: &Demand) -> bool {
        (self.0)(demand)
    }
}

impl fmt::Debug for Launchable {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Launchable(..)")
    }
}

impl Registry {
    /// The whole-node requests that found no host and still wait, front first, each with
    /// the host it holds a mark on, if any. A request is on the list from the first start
    /// that finds no free host, and leaves it when it is placed, cancelled, refused, or
    /// not presented for the pool's `regain_grace` (see [`Registry::queue_position`]).
    pub fn waiting(&self) -> Vec<Waiting> {
        let state = self.lock();
        let waiting = state.waiting.waiting();
        let rows = waiting.map(|(id, request, since)| Waiting {
            id: id.clone(),
            demand: Demand::from_request(request),
            since,
            reserved: state.marks.host_of(id),
        });
        rows.collect()
    }

    /// Sets, or with `None` removes, the answer to "could a launch serve this request".
    /// With one set, a whole-node request that no listed host could ever serve is still
    /// capable when `launchable` says yes, so it waits instead of being refused
    /// `no-capable-host`. Without one, only listed hosts count, as before.
    pub fn set_launchable(&self, launchable: Option<Launchable>) {
        self.lock().launchable = launchable;
    }
}
