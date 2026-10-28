//! The answer to a whole-node start that found no free host.

use std::collections::BTreeSet;

use js_lease::Timestamp;
use js_provider::{HostCapacity, RefusalCode, StartAck, StartSpec};
use js_types::HostId;

use super::queue::{Verdict, WaitList};
use super::requests::{RequestKey, Requests};

/// What a declined start is answered from.
pub(crate) struct Declined<'a> {
    pub(crate) spec: &'a StartSpec,
    pub(crate) key: &'a RequestKey,
    pub(crate) now: Timestamp,
    pub(crate) regain_grace_ms: u64,
    /// Every host the pool lists.
    pub(crate) hosts: &'a [HostCapacity],
    /// The hosts that reported a taint.
    pub(crate) tainted: &'a BTreeSet<HostId>,
}

/// Whether some listed host could ever be given whole to the request: it fits the
/// request's shape, is untainted and is not barred for the request's life. A busy, lost
/// or reserved host counts as capable.
fn capable(declined: &Declined<'_>, requests: &Requests) -> bool {
    let wanted = declined.spec.request();
    declined.hosts.iter().any(|host| {
        let fits = host
            .shape
            .satisfies(&wanted.resources, &wanted.capabilities);
        let clean = !declined.tainted.contains(&host.host);
        let barred = requests.barred_for_life(declined.key, &host.host);
        fits & clean & !barred
    })
}

/// The answer to a whole-node start of a request that fits the pool but found no free
/// host: no host could ever be given whole is the one refusal for lack of a host;
/// otherwise a request with an id waits on the list (`Unknown`, or `NoCapacity` when the
/// list is full) and one without an id is answered `NoCapacity`.
pub(crate) fn answer(
    waiting: &mut WaitList,
    requests: &Requests,
    declined: &Declined<'_>,
) -> StartAck {
    let capable = capable(declined, requests);
    let place = declined
        .spec
        .request_id()
        .map(|id| waiting.present(id, capable, declined.now, declined.regain_grace_ms));
    match (place, capable) {
        (Some(Verdict::Waiting(_)), _) => StartAck::Unknown,
        (Some(Verdict::Full), _) => StartAck::Refused(RefusalCode::NoCapacity),
        (Some(Verdict::Unservable), _) | (None, false) => {
            StartAck::Refused(RefusalCode::NoCapableHost)
        }
        (None, true) => StartAck::Refused(RefusalCode::NoCapacity),
    }
}
