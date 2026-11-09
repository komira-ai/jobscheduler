//! The placer the registry tells of its hosts.

use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Shape};

/// What the registry tells its placer of its hosts, and asks it.
///
/// The registry keeps the admission of hosts, their sessions, taints, cordons and marks; the
/// placer keeps the picture of the hosts that placing jobs needs. The registry tells it when a
/// host says hello ([`Placer::host_up`]), when a heartbeat is accepted ([`Placer::heard`]) and
/// when a host is forgotten or given back ([`Placer::host_gone`]), always under the
/// registry's own lock; and it asks which hosts the placer holds work on
/// ([`Placer::holds_work_on`]), so that such a host is not forgotten for being silent. Every
/// registry has one, given at construction ([`crate::Registry::new`]).
///
/// Why a trait here: the placer that ships, `SchedPlacer` of `js-sched-placer`, keeps a
/// `js-sched` scheduler for the life of the server and maps hosts into it with that crate's
/// own conversions. That crate depends on this one, so the registry cannot name that placer;
/// it can only hold something the server hands it, which is this seam.
pub trait Placer: Send + Sync {
    /// `host` said hello and is listed, booked as `shape`, the pool's shape: a new host,
    /// or one that comes back. Told under the registry's lock. The default does nothing.
    fn host_up(&self, host: &HostId, shape: &Shape) {
        let _ = (host, shape);
    }

    /// The registry accepted a heartbeat of `host`; `load` is the reading it brought, as
    /// the registry vetted and converted it, when it brought a new one that passed (a beat
    /// with no reading, a repeated one, or one the vet drops gives `None`). Told under the
    /// registry's lock. The default does nothing.
    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        let _ = (host, load);
    }

    /// `host` is no longer listed: it was lost past the pool's retention holding no work of
    /// the placer's, or given back ([`crate::Registry::remove`]). Told under the registry's
    /// lock. The default does nothing.
    fn host_gone(&self, host: &HostId) {
        let _ = host;
    }

    /// Whether the placer holds work on `host` (a job's allocation): such a host is not
    /// forgotten however long it is silent, since only its own confirmation or the
    /// provider's absence frees that work. Asked under the registry's lock. The default,
    /// `false`, holds nothing there.
    fn holds_work_on(&self, host: &HostId) -> bool {
        let _ = host;
        false
    }
}

#[cfg(test)]
pub(crate) struct Plain;

#[cfg(test)]
impl Placer for Plain {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_types::Resources;

    use super::*;

    /// A placer that takes the defaults tells and holds nothing: it is told of a host, a
    /// beat and a loss without effect, and holds no work on any host.
    #[test]
    fn a_placer_that_takes_the_defaults_holds_nothing() {
        let host = HostId::new("a").unwrap();
        let shape = Shape::new(
            Resources::new(1000, 1 << 30, 0, None).unwrap(),
            BTreeMap::new(),
        );
        let placer = Plain;
        placer.host_up(&host, &shape);
        placer.heard(&host, None);
        placer.host_gone(&host);
        assert!(!placer.holds_work_on(&host));
    }
}
