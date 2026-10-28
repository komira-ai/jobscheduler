//! The answer to a whole-node start that found no free host.

use std::collections::BTreeSet;

use js_provider::{HostCapacity, RefusalCode, StartAck, StartSpec};
use js_types::HostId;

use super::queue::Verdict;
use super::requests::{RequestKey, Requests};

/// What decides whether some host could ever be given whole to a request.
pub(crate) struct Asking<'a> {
    pub(crate) spec: &'a StartSpec,
    pub(crate) key: &'a RequestKey,
    /// Every host the pool lists.
    pub(crate) hosts: &'a [HostCapacity],
    /// The hosts that reported a taint.
    pub(crate) tainted: &'a BTreeSet<HostId>,
}

/// Whether some listed host could ever be given whole to the request: it fits the
/// request's shape, is untainted and is not barred for the request's life. A busy, lost
/// or reserved host counts as capable.
pub(crate) fn capable(asking: &Asking<'_>, requests: &Requests) -> bool {
    let wanted = asking.spec.request();
    asking.hosts.iter().any(|host| {
        let fits = host
            .shape
            .satisfies(&wanted.resources, &wanted.capabilities);
        let clean = !asking.tainted.contains(&host.host);
        let barred = requests.barred_for_life(asking.key, &host.host);
        fits & clean & !barred
    })
}

/// The answer to a whole-node start of a request that fits the pool but found no free
/// host. `verdict` is the wait list's answer for a request with an id that may queue,
/// else `None`. No host that could ever be given whole is the one refusal for lack of a
/// host; otherwise a request on the list waits (`Unknown`), one the list is full for is
/// answered `NoCapacity`, and so is one that does not queue.
pub(crate) fn answer(verdict: Option<Verdict>, capable: bool) -> StartAck {
    match (verdict, capable) {
        (Some(Verdict::Waiting(_)), _) => StartAck::Unknown,
        (Some(Verdict::Full), _) => StartAck::Refused(RefusalCode::NoCapacity),
        (Some(Verdict::Unservable), _) | (None, false) => {
            StartAck::Refused(RefusalCode::NoCapableHost)
        }
        (None, true) => StartAck::Refused(RefusalCode::NoCapacity),
    }
}
