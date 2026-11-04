//! The answer to a whole-node start that found no free host.

use std::collections::BTreeSet;

use js_provider::{HostCapacity, RefusalCode, StartAck, StartSpec};
use js_types::{Capabilities, HostId, Resources};

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

/// Whether some listed host could ever be given whole to the request: it could hold it
/// ([`could_hold`]) and is not barred for the request's life. A busy, lost or reserved
/// host counts as capable.
pub(crate) fn capable(asking: &Asking<'_>, requests: &Requests) -> bool {
    let wanted = asking.spec.request();
    asking.hosts.iter().any(|host| {
        let holds = could_hold(
            host,
            &wanted.resources,
            &wanted.capabilities,
            asking.tainted,
        );
        let barred = requests.barred_for_life(asking.key, &host.host);
        holds & !barred
    })
}

/// Whether `host` could ever hold a request for `resources` that requires `required`: it
/// fits them and is not in `tainted`. Whether it is busy, lost or reserved does not
/// matter: it could free.
pub(crate) fn could_hold(
    host: &HostCapacity,
    resources: &Resources,
    required: &Capabilities,
    tainted: &BTreeSet<HostId>,
) -> bool {
    let fits = host.shape.satisfies(resources, required);
    fits & !tainted.contains(&host.host)
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
