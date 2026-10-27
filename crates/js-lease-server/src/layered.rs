//! Several sources of hosts behind the one provider the lease table runs on.

use std::collections::{BTreeMap, BTreeSet};

use js_lease::Booking;
use js_provider::{
    Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_registry::{Binding, Registry};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

use crate::capacity::{CapacitySource, Seated, Unseated};
use crate::error::StartupError;

/// The static pool (when there is one) and the registered hosts, as one provider.
///
/// - **Start** asks the child that already holds the lease, if there is one, so a
///   repeated start never books a second host. Otherwise it asks the children in order,
///   the static pool first, and moves on to the next only when a child has no room for
///   the request ([`RefusalCode::NoCapacity`], [`RefusalCode::NoFit`]) or does not serve
///   its class ([`RefusalCode::UnsupportedClass`]). Any other answer is final: a refusal
///   of the start contract would be the same on every child, an ended lease is ended
///   everywhere, and an unanswered start must not be repeated on another child. When
///   every child declines, the answer is the most hopeful of the declines: no capacity
///   (a host could free up), else no fit, else unsupported class.
/// - **Stop** goes to every child, since each remembers the ended lease.
/// - **Observe** and the read-back of a stop are `Present` if any child says so, else
///   `Unknown` if any does, else `Absent`.
/// - **Capacity** lists every child's hosts by id. Host ids are unique across children:
///   the registry refuses a hello for an id the static pool lists.
///
/// Public (and hidden from the documentation) only so that the conformance suite can
/// run over a layer of a static pool and a registry; the server builds it through
/// `LayeredSource`.
pub struct LayeredProvider {
    id: ProviderId,
    children: Vec<Box<dyn Provider + Send>>,
}

impl LayeredProvider {
    /// A layer over `children`, asked in order.
    pub fn new(id: ProviderId, children: Vec<Box<dyn Provider + Send>>) -> Self {
        Self { id, children }
    }
}

/// How hopeful a decline is; the layered answer is the highest of the children's.
fn hope(code: RefusalCode) -> u8 {
    match code {
        RefusalCode::NoCapacity => 2,
        RefusalCode::NoFit => 1,
        _ => 0,
    }
}

/// The read-back of several children: `Present` beats `Unknown`, which beats `Absent`.
fn combine(reads: impl IntoIterator<Item = Observed>) -> Observed {
    let mut combined = Observed::Absent;
    for read in reads {
        combined = match (combined, read) {
            (present @ Observed::Present { .. }, _) | (_, present @ Observed::Present { .. }) => {
                present
            }
            (Observed::Unknown, _) | (_, Observed::Unknown) => Observed::Unknown,
            (Observed::Absent, Observed::Absent) => Observed::Absent,
        };
    }
    combined
}

impl Provider for LayeredProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn flavour(&self) -> Flavour {
        Flavour::Delegated
    }

    fn classes(&self) -> Vec<ResourceClass> {
        let mut classes: Vec<ResourceClass> = Vec::new();
        for class in self.children.iter().flat_map(|child| child.classes()) {
            if !classes.contains(&class) {
                classes.push(class);
            }
        }
        classes
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        let lease = spec.lease();
        let holder = self.children.iter().position(|child| {
            let capacity = child.capacity();
            capacity.iter().any(|host| host.leased_to == Some(lease))
        });
        if let Some(holder) = holder {
            return self.children[holder].start(spec);
        }
        let mut declined = RefusalCode::UnsupportedClass;
        for child in &mut self.children {
            match child.start(spec.clone()) {
                StartAck::Refused(
                    code @ (RefusalCode::NoCapacity
                    | RefusalCode::NoFit
                    | RefusalCode::UnsupportedClass),
                ) => {
                    if hope(code) > hope(declined) {
                        declined = code;
                    }
                }
                settled => return settled,
            }
        }
        StartAck::Refused(declined)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        let reads = self
            .children
            .iter_mut()
            .map(|child| match child.stop(lease) {
                StopAck::Stopped { read_back } => read_back,
                StopAck::Unknown => Observed::Unknown,
            })
            .collect::<Vec<_>>();
        StopAck::Stopped {
            read_back: combine(reads),
        }
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        let reads = self
            .children
            .iter_mut()
            .map(|child| child.observe(lease))
            .collect::<Vec<_>>();
        combine(reads)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        let mut hosts: Vec<HostCapacity> = self
            .children
            .iter()
            .flat_map(|child| child.capacity())
            .collect();
        hosts.sort_by(|a, b| a.host.cmp(&b.host));
        hosts
    }
}

/// Builds the layered provider: the static pool from its source, and the registered
/// hosts from the registry, with the lease file's bookings put back in whichever of the
/// two lists the host.
pub(crate) struct LayeredSource {
    static_pool: Option<Box<dyn CapacitySource>>,
    registry: Registry,
    bindings: BTreeMap<HostId, Binding>,
}

impl LayeredSource {
    /// `bindings` are what the previous run persisted for the hosts it admitted.
    pub(crate) fn new(
        static_pool: Option<Box<dyn CapacitySource>>,
        registry: Registry,
        bindings: BTreeMap<HostId, Binding>,
    ) -> Self {
        Self {
            static_pool,
            registry,
            bindings,
        }
    }
}

impl CapacitySource for LayeredSource {
    /// The static pool seats the bookings of the hosts it lists. Every other booking is
    /// seated on the registry as an awaiting host, with the binding persisted for it,
    /// and what the registry refuses to seat comes back unseated. A booking of a host
    /// the static pool lists but could not seat stays unseated, and is not offered to
    /// the registry: an id belongs to one source only, and the registry reserves the
    /// static pool's ids so that no hello can take one.
    fn provider(&self, bookings: &[Booking]) -> Result<Seated, StartupError> {
        let mut children: Vec<Box<dyn Provider + Send>> = Vec::new();
        let mut unseated = Vec::new();
        let mut listed = BTreeSet::new();
        if let Some(source) = &self.static_pool {
            let Seated {
                provider,
                unseated: refused,
            } = source.provider(bookings)?;
            listed.extend(provider.capacity().into_iter().map(|host| host.host));
            let owned = |refusal: &Unseated| listed.contains(&refusal.booking.host);
            unseated.extend(refused.into_iter().filter(owned));
            children.push(provider);
        }
        let rest = bookings
            .iter()
            .filter(|booking| !listed.contains(&booking.host));
        let rest = rest.collect::<Vec<_>>();
        self.registry.reserve(listed);
        let registered = self.registry.provider();
        for booking in rest {
            let binding = self.bindings.get(&booking.host).cloned();
            if let Err(refused) = registered.seat(&booking.host, booking.lease, binding) {
                unseated.push(Unseated {
                    booking: booking.clone(),
                    reason: refused.to_string(),
                });
            }
        }
        children.push(Box::new(registered));
        let id =
            ProviderId::new("layered").map_err(StartupError::because("--agent-pool-config"))?;
        Ok(Seated {
            provider: Box::new(LayeredProvider::new(id, children)),
            unseated,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use js_provider::Call;
    use js_provider_fake::FakeProvider;
    use js_types::{Admission, ClassId, Request, Resources, Shape};

    use super::*;

    fn host(name: &str) -> HostId {
        HostId::new(name).unwrap()
    }

    fn shape(cpu: u64, gpus: u64) -> Shape {
        let class = (gpus > 0).then(|| "a100".to_string());
        Shape::new(
            Resources::new(cpu, 1 << 30, gpus, class).unwrap(),
            BTreeMap::new(),
        )
    }

    fn fake(name: &str, hosts: &[(&str, u64, u64)]) -> FakeProvider {
        let hosts = hosts
            .iter()
            .map(|(id, cpu, gpus)| (host(id), shape(*cpu, *gpus)))
            .collect();
        FakeProvider::new(ProviderId::new(name).unwrap(), hosts)
    }

    fn mac() -> ResourceClass {
        ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0)
    }

    fn layered(children: Vec<FakeProvider>) -> LayeredProvider {
        let children = children
            .into_iter()
            .map(|child| Box::new(child) as Box<dyn Provider + Send>)
            .collect();
        LayeredProvider::new(ProviderId::new("layered").unwrap(), children)
    }

    /// A start of `sequence` for a whole host with at least `cpu` and `gpus`.
    fn spec(sequence: u64, cpu: u64, gpus: u64) -> StartSpec {
        let class = (gpus > 0).then(|| "a100".to_string());
        let request = Request {
            class: ClassId::new("gpu-host").unwrap(),
            resources: Resources::new(cpu, 1, gpus, class).unwrap(),
            capabilities: BTreeMap::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
        };
        StartSpec::new(
            LeaseId::new(1, sequence),
            request,
            &[ResourceClass::gpu_host()],
        )
        .unwrap()
    }

    fn mac_spec(sequence: u64, cpu: u64) -> StartSpec {
        let request = Request {
            class: ClassId::new("mac-host").unwrap(),
            resources: Resources::new(cpu, 1, 0, None).unwrap(),
            capabilities: BTreeMap::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
        };
        StartSpec::new(LeaseId::new(1, sequence), request, &[mac()]).unwrap()
    }

    /// The answer of a start that booked `name`.
    fn on(name: &str) -> StartAck {
        StartAck::Started { host: host(name) }
    }

    fn leased(provider: &LayeredProvider, name: &str) -> Option<LeaseId> {
        let hosts = provider.capacity();
        hosts.into_iter().find(|h| h.host == host(name))?.leased_to
    }

    /// Hosts are taken from the first child until it is full and then from the next, and
    /// when no child has room the answer is a refusal of capacity. It catches a layer
    /// that spreads leases over the children or that never reaches the second.
    #[test]
    fn the_first_child_fills_before_the_next() {
        let mut provider = layered(vec![
            fake("a", &[("a-1", 1000, 1)]),
            fake("b", &[("b-1", 1000, 1)]),
        ]);
        let first = provider.start(spec(1, 1, 1));
        assert_eq!(first, on("a-1"));
        let second = provider.start(spec(2, 1, 1));
        assert_eq!(second, on("b-1"));
        let full = provider.start(spec(3, 1, 1));
        assert_eq!(full, StartAck::Refused(RefusalCode::NoCapacity));
    }

    /// When every child declines, the answer is the most hopeful decline whatever the
    /// order of the children: no capacity over no fit over an unsupported class. It
    /// catches an answer taken from the last child only, which would tell a client there
    /// is no host of that size when one exists and is busy.
    #[test]
    fn a_decline_is_the_most_hopeful_of_the_children() {
        for flip in [false, true] {
            let small = fake("small", &[("s-1", 1000, 1)]);
            let big = fake("big", &[("b-1", 8000, 1)]);
            let mut children = vec![small, big];
            if flip {
                children.reverse();
            }
            let mut provider = layered(children);
            let fits = provider.start(spec(1, 4000, 1));
            assert_eq!(fits, on("b-1"));
            let busy = provider.start(spec(2, 4000, 1));
            assert_eq!(busy, StartAck::Refused(RefusalCode::NoCapacity));
            let nowhere = provider.start(spec(3, 9000, 1));
            assert_eq!(nowhere, StartAck::Refused(RefusalCode::NoFit));
        }
        let unsupported = |name| fake(name, &[("h", 1000, 1)]).with_unleasable_class(mac());
        let mut provider = layered(vec![unsupported("a"), unsupported("b")]);
        let declined = provider.start(mac_spec(1, 1));
        assert_eq!(declined, StartAck::Refused(RefusalCode::UnsupportedClass));
        let mut provider = layered(vec![unsupported("a"), fake("b", &[("h2", 1000, 1)])]);
        let too_big = provider.start(mac_spec(2, 5000));
        assert_eq!(too_big, StartAck::Refused(RefusalCode::NoFit));
    }

    /// A child that does not serve the class hands the start on, and the next child
    /// books it: an unsupported class is a decline, not an answer.
    #[test]
    fn an_unsupported_class_moves_on() {
        let mut provider = layered(vec![
            fake("a", &[("a-1", 1000, 1)]).with_unleasable_class(mac()),
            fake("b", &[("b-1", 1000, 1)]),
        ]);
        let ack = provider.start(mac_spec(1, 1));
        assert_eq!(ack, on("b-1"));
    }

    /// A refusal of the start contract is final: a lease that one child knows as ended is
    /// refused as ended and the next child is not tried, where it would book the lease a
    /// stopped id may never have. It catches a layer that retries every refusal.
    #[test]
    fn a_contract_refusal_is_not_retried_on_the_next_child() {
        let mut first = fake("a", &[("a-1", 1000, 1)]);
        first.stop(&LeaseId::new(1, 1));
        let mut provider = layered(vec![first, fake("b", &[("b-1", 1000, 1)])]);
        let ack = provider.start(spec(1, 1, 1));
        assert_eq!(ack, StartAck::Refused(RefusalCode::LeaseEnded));
        assert_eq!(leased(&provider, "b-1"), None);
    }

    /// A start that no child answered is not repeated on the next one, since the first
    /// may still book it. It catches a layer that books a lease twice after a timeout.
    #[test]
    fn an_unanswered_start_is_not_repeated_on_the_next_child() {
        let mut first = fake("a", &[("a-1", 1000, 1)]);
        first.script_unknown(Call::Start, false);
        let mut provider = layered(vec![first, fake("b", &[("b-1", 1000, 1)])]);
        assert_eq!(provider.start(spec(1, 1, 1)), StartAck::Unknown);
        assert_eq!(leased(&provider, "b-1"), None);
    }

    /// A repeated start goes to the child that holds the lease, even when an earlier
    /// child has since got a free host. It catches a layer that books a lease on two
    /// hosts when the static pool frees up after a registered host took the lease.
    #[test]
    fn a_repeated_start_goes_to_the_child_that_holds_the_lease() {
        let mut provider = layered(vec![
            fake("a", &[("a-1", 1000, 1)]),
            fake("b", &[("b-1", 1000, 1)]),
        ]);
        provider.start(spec(1, 1, 1));
        let second = provider.start(spec(2, 1, 1));
        assert_eq!(second, on("b-1"));
        provider.stop(&LeaseId::new(1, 1));
        assert_eq!(leased(&provider, "a-1"), None);
        let again = provider.start(spec(2, 1, 1));
        assert_eq!(again, on("b-1"));
        assert_eq!(leased(&provider, "a-1"), None);
    }

    /// A stop reaches every child, not only the one that holds the lease, so each
    /// remembers the lease as ended: a late start of the id is refused as ended by the
    /// first child instead of booking it there. The read-back is absent.
    #[test]
    fn a_stop_reaches_every_child() {
        let mut provider = layered(vec![
            fake("a", &[("a-1", 1000, 1)]),
            fake("b", &[("b-1", 1000, 1)]),
        ]);
        provider.start(spec(1, 1, 1));
        provider.start(spec(2, 1, 1));
        let stopped = provider.stop(&LeaseId::new(1, 2));
        assert_eq!(
            stopped,
            StopAck::Stopped {
                read_back: Observed::Absent
            }
        );
        assert_eq!(leased(&provider, "b-1"), None);
        let late = provider.start(spec(2, 1, 1));
        assert_eq!(late, StartAck::Refused(RefusalCode::LeaseEnded));
        assert_eq!(leased(&provider, "a-1"), Some(LeaseId::new(1, 1)));
    }

    /// The read-back of several children is present if any child says so, else unknown
    /// if any does, else absent, for an observe and for a stop alike. It catches a layer
    /// that lets one child's absent free a lease another child still holds or cannot
    /// reach.
    #[test]
    fn present_beats_unknown_beats_absent() {
        let mut silent = fake("a", &[("a-1", 1000, 1)]);
        silent.script_unknown(Call::Observe, false);
        silent.script_unknown(Call::Observe, false);
        silent.script_unknown(Call::Stop, false);
        let mut provider = layered(vec![silent, fake("b", &[("b-1", 1000, 1)])]);
        provider.children[1].start(spec(1, 1, 1));
        let held = LeaseId::new(1, 1);
        assert_eq!(
            provider.observe(&held),
            Observed::Present { host: host("b-1") }
        );
        let never = LeaseId::new(1, 9);
        assert_eq!(provider.observe(&never), Observed::Unknown);
        assert_eq!(provider.observe(&never), Observed::Absent);
        let stopped = provider.stop(&never);
        assert_eq!(
            stopped,
            StopAck::Stopped {
                read_back: Observed::Unknown
            }
        );
    }

    /// The layer's identity: delegated, the union of the children's classes without
    /// repeats, and every child's hosts in id order whichever child lists them.
    #[test]
    fn the_layer_lists_classes_and_hosts_of_all_children() {
        let provider = layered(vec![
            fake("a", &[("z-1", 1000, 1)]).with_unleasable_class(mac()),
            fake("b", &[("a-1", 1000, 1)]),
        ]);
        assert_eq!(provider.id().as_str(), "layered");
        assert_eq!(provider.flavour(), Flavour::Delegated);
        assert_eq!(provider.classes(), vec![ResourceClass::gpu_host(), mac()]);
        let ids: Vec<_> = provider.capacity().into_iter().map(|h| h.host).collect();
        assert_eq!(ids, vec![host("a-1"), host("z-1")]);
    }
}
