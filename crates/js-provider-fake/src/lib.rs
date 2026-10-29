//! A fake delegated provider: an in-memory host table with scripted Unknown answers
//! and switchable defects.
//!
//! The fake is the executable spec of the delegated flavour (design 13.1). Script an
//! Unknown with [`FakeProvider::script_unknown`] to see a timeout: the call answers
//! `Unknown`, and the effect lands or not as scripted. Switch on a [`Defect`] to build
//! a deliberately broken provider; the conformance suite must report exactly the
//! rules that defect breaks (13.2), which is how the suite is shown able to fail.
//! The fake remembers every lease id it has stopped, as an exact set, and refuses a
//! start of one with `LeaseEnded`; see the static pool for why the set and not a
//! high-water mark.
//! [`FakeProvider::booked`] reads the host table directly, for conformance hooks.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{BTreeMap, BTreeSet};

use js_provider::{
    Call, Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_types::{HostId, LeaseId, MemoryHeadroom, ProviderId, ResourceClass, Shape};

/// Which free fitting host the fake picks first (design 6.1: rank is the provider's).
/// An option, not a defect: both orders are conformant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rank {
    /// The lowest host id.
    #[default]
    LowestHostFirst,
    /// The highest host id.
    HighestHostFirst,
}

/// A deliberate fault, for showing the conformance suite can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// An Unknown observation frees the booking.
    FreesOnUnknownObserve,
    /// A new lease may take a host that is already booked.
    DoubleBooks,
    /// A second start of the same lease books a second host.
    RestartBooksAgain,
    /// A stop leaves the host booked but reads back Absent.
    StopKeepsBooking,
    /// A start that answered Unknown but landed is not found by the retry, which books
    /// a second host instead of answering the lease's first one.
    ForgetsUnknownStart,
    /// An observation of a booked lease answers Absent.
    ObservesAbsent,
    /// A start of a lease that was stopped books a host again, as a late or duplicate
    /// start would.
    ResurrectsStoppedLease,
    /// A start books a free host without checking that the host satisfies the request.
    IgnoresShape,
    /// A start of a class the fake lists but cannot lease books a whole host anyway.
    LeasesUnsupportedClass,
}

#[derive(Debug)]
struct Host {
    shape: Shape,
    lease: Option<LeaseId>,
}

/// An in-memory delegated provider serving the `gpu-host` class.
#[derive(Debug)]
pub struct FakeProvider {
    id: ProviderId,
    hosts: BTreeMap<HostId, Host>,
    script: Vec<(Call, bool)>,
    unacked: Vec<LeaseId>,
    ended: BTreeSet<LeaseId>,
    rank: Rank,
    unleasable: Vec<ResourceClass>,
    extra_classes: Vec<ResourceClass>,
    defect: Option<Defect>,
}

impl FakeProvider {
    /// A fake over these hosts, all free, with no script and no defect.
    pub fn new(id: ProviderId, hosts: Vec<(HostId, Shape)>) -> Self {
        let hosts = hosts
            .into_iter()
            .map(|(id, shape)| (id, Host { shape, lease: None }));
        Self {
            id,
            hosts: hosts.collect(),
            script: Vec::new(),
            unacked: Vec::new(),
            ended: BTreeSet::new(),
            rank: Rank::default(),
            unleasable: Vec::new(),
            extra_classes: Vec::new(),
            defect: None,
        }
    }

    /// The same fake with a defect switched on.
    pub fn with_defect(mut self, defect: Defect) -> Self {
        self.defect = Some(defect);
        self
    }

    /// The same fake choosing among free fitting hosts by `rank`.
    pub fn with_rank(mut self, rank: Rank) -> Self {
        self.rank = rank;
        self
    }

    /// The same fake also listing `class`, which it cannot lease: a start of it is refused
    /// with `UnsupportedClass` and books nothing.
    pub fn with_unleasable_class(mut self, class: ResourceClass) -> Self {
        self.unleasable.push(class);
        self
    }

    /// The same fake also serving `class`, which it leases like `gpu-host`: a start of it
    /// books a fitting host. For a test that needs a class of another admission mode.
    pub fn with_class(mut self, class: ResourceClass) -> Self {
        self.extra_classes.push(class);
        self
    }

    /// Makes the next `call` answer Unknown. For a start or a stop, `landed` says
    /// whether the effect still happened; it is ignored for an observe.
    pub fn script_unknown(&mut self, call: Call, landed: bool) {
        self.script.push((call, landed));
    }

    /// The hosts that are booked and the lease on each, read from the host table.
    pub fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        let held = self
            .hosts
            .iter()
            .filter_map(|(id, host)| Some((id.clone(), host.lease?)));
        held.collect()
    }

    fn take_script(&mut self, call: Call) -> Option<bool> {
        let at = self
            .script
            .iter()
            .position(|(scripted, _)| *scripted == call)?;
        Some(self.script.remove(at).1)
    }

    /// Whether this retry of a start must ignore the lease's booking: the defect is on
    /// and an earlier start of the lease answered Unknown. Consumes that memory.
    fn forgets(&mut self, lease: &LeaseId) -> bool {
        let before = self.unacked.len();
        self.unacked.retain(|unacked| unacked != lease);
        let was_unacked = self.unacked.len() != before;
        [was_unacked, self.defect_is(Defect::ForgetsUnknownStart)]
            .into_iter()
            .all(|yes| yes)
    }

    fn defect_is(&self, defect: Defect) -> bool {
        self.defect == Some(defect)
    }

    fn holder(&self, lease: &LeaseId) -> Option<HostId> {
        let held = self
            .hosts
            .iter()
            .find(|(_, host)| host.lease.as_ref() == Some(lease));
        held.map(|(id, _)| id.clone())
    }

    fn read(&self, lease: &LeaseId) -> Observed {
        match self.holder(lease) {
            Some(host) => Observed::Present { host },
            None => Observed::Absent,
        }
    }

    fn free(&mut self, lease: &LeaseId) {
        for host in self.hosts.values_mut() {
            if host.lease.as_ref() == Some(lease) {
                host.lease = None;
            }
        }
    }

    fn book(&mut self, spec: &StartSpec) -> StartAck {
        let ended = self.ended.contains(&spec.lease());
        let remembers = !self.defect_is(Defect::ResurrectsStoppedLease);
        if [ended, remembers].into_iter().all(|yes| yes) {
            return StartAck::Refused(RefusalCode::LeaseEnded);
        }
        let repeats_booking = self.defect_is(Defect::RestartBooksAgain);
        let forgotten = self.forgets(&spec.lease());
        let blind = [repeats_booking, forgotten].into_iter().any(|yes| yes);
        let already = self.holder(&spec.lease()).filter(|_| !blind);
        if let Some(host) = already {
            return StartAck::Started { host };
        }
        let lists_it = self.unleasable.contains(spec.class());
        let refuses_it = !self.defect_is(Defect::LeasesUnsupportedClass);
        if [lists_it, refuses_it].into_iter().all(|yes| yes) {
            return StartAck::Refused(RefusalCode::UnsupportedClass);
        }
        let wanted = spec.request();
        let double_books = self.defect_is(Defect::DoubleBooks);
        let ignores_shape = self.defect_is(Defect::IgnoresShape);
        let fits = |host: &Host| {
            let satisfied = host
                .shape
                .satisfies(&wanted.resources, &wanted.capabilities);
            [satisfied, ignores_shape].into_iter().any(|ok| ok)
        };
        let open = |host: &Host| {
            [host.lease.is_none(), double_books]
                .into_iter()
                .any(|ok| ok)
        };
        let fits_somewhere = self.hosts.values().any(&fits);
        let mut order: Vec<_> = self.hosts.iter_mut().collect();
        if self.rank == Rank::HighestHostFirst {
            order.reverse();
        }
        let target = order
            .into_iter()
            .find(|(_, host)| [open(host), fits(host)].into_iter().all(|ok| ok));
        match target {
            Some((id, host)) => {
                host.lease = Some(spec.lease());
                StartAck::Started { host: id.clone() }
            }
            None if fits_somewhere => StartAck::Refused(RefusalCode::NoCapacity),
            None => StartAck::Refused(RefusalCode::NoFit),
        }
    }
}

impl Provider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn flavour(&self) -> Flavour {
        Flavour::Delegated
    }

    fn classes(&self) -> Vec<ResourceClass> {
        let mut listed = vec![ResourceClass::gpu_host()];
        listed.extend(self.extra_classes.iter().cloned());
        listed.extend(self.unleasable.iter().cloned());
        listed
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        let Some(landed) = self.take_script(Call::Start) else {
            return self.book(&spec);
        };
        if landed {
            self.book(&spec);
            self.unacked.push(spec.lease());
        }
        StartAck::Unknown
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        let Some(landed) = self.take_script(Call::Stop) else {
            return self.stop_now(lease);
        };
        if landed {
            self.stop_now(lease);
        }
        StopAck::Unknown
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        if self.take_script(Call::Observe).is_none() {
            let seen = self.read(lease);
            return match self.defect_is(Defect::ObservesAbsent) {
                true => Observed::Absent,
                false => seen,
            };
        }
        if self.defect_is(Defect::FreesOnUnknownObserve) {
            self.free(lease);
        }
        Observed::Unknown
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        let listed = self.hosts.iter().map(|(id, host)| HostCapacity {
            host: id.clone(),
            shape: host.shape.clone(),
            memory: MemoryHeadroom::full(host.shape.resources().memory_bytes()),
            leased_to: host.lease,
        });
        listed.collect()
    }
}

impl FakeProvider {
    fn stop_now(&mut self, lease: &LeaseId) -> StopAck {
        self.ended.insert(*lease);
        if self.defect_is(Defect::StopKeepsBooking) {
            return StopAck::Stopped {
                read_back: Observed::Absent,
            };
        }
        self.free(lease);
        StopAck::Stopped {
            read_back: self.read(lease),
        }
    }
}
