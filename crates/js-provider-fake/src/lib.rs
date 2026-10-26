//! A fake delegated provider: an in-memory host table with scripted Unknown answers
//! and switchable defects.
//!
//! The fake is the executable spec of the delegated flavour (design 13.1). Script an
//! Unknown with [`FakeProvider::script_unknown`] to see a timeout: the call answers
//! `Unknown`, and the effect lands or not as scripted. Switch on a [`Defect`] to build
//! a deliberately broken provider; the conformance suite must report exactly the
//! rule that defect breaks (13.2), which is how the suite is shown able to fail.
//! [`FakeProvider::booked`] reads the host table directly, for conformance hooks.

use std::collections::BTreeMap;

use js_provider::{
    Call, Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

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
            defect: None,
        }
    }

    /// The same fake with a defect switched on.
    pub fn with_defect(mut self, defect: Defect) -> Self {
        self.defect = Some(defect);
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
        let repeats_booking = self.defect_is(Defect::RestartBooksAgain);
        let already = self.holder(&spec.lease()).filter(|_| !repeats_booking);
        if let Some(host) = already {
            return StartAck::Started { host };
        }
        let wanted = spec.request();
        let double_books = self.defect_is(Defect::DoubleBooks);
        let open = |host: &Host| {
            [host.lease.is_none(), double_books]
                .into_iter()
                .any(|ok| ok)
        };
        let target = self.hosts.iter_mut().find(|(_, host)| {
            [
                open(host),
                host.shape
                    .satisfies(&wanted.resources, &wanted.capabilities),
            ]
            .into_iter()
            .all(|ok| ok)
        });
        match target {
            Some((id, host)) => {
                host.lease = Some(spec.lease());
                StartAck::Started { host: id.clone() }
            }
            None => StartAck::Refused(RefusalCode::NoCapacity),
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
        vec![ResourceClass::gpu_host()]
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        let Some(landed) = self.take_script(Call::Start) else {
            return self.book(&spec);
        };
        if landed {
            self.book(&spec);
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
            return self.read(lease);
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
            leased_to: host.lease,
        });
        listed.collect()
    }
}

impl FakeProvider {
    fn stop_now(&mut self, lease: &LeaseId) -> StopAck {
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
