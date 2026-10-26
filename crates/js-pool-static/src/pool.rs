use std::collections::BTreeMap;
use std::path::Path;

use js_provider::{
    Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_types::{Admission, HostId, LeaseId, ProviderId, ResourceClass, Shape};

use crate::config::{self, ConfigError};

#[derive(Debug)]
struct Host {
    shape: Shape,
    lease: Option<LeaseId>,
}

/// A provider over a fixed list of hosts, each leased whole. See the crate docs for
/// the placement rules.
#[derive(Debug)]
pub struct StaticPool {
    id: ProviderId,
    classes: Vec<ResourceClass>,
    hosts: BTreeMap<HostId, Host>,
}

impl StaticPool {
    /// A pool over configuration text.
    pub fn from_toml(id: ProviderId, source: &str) -> Result<Self, ConfigError> {
        let hosts = config::parse(source)?;
        let hosts = hosts
            .into_iter()
            .map(|(host, shape)| (host, Host { shape, lease: None }));
        Ok(Self {
            id,
            classes: vec![ResourceClass::gpu_host()],
            hosts: hosts.collect(),
        })
    }

    /// A pool over the configuration file at `path`.
    pub fn from_config_file(id: ProviderId, path: &Path) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(id, &source)
    }

    /// The same pool serving `classes` instead of the shipped one. The pool leases a
    /// whole host whatever the class says, so `start` refuses, with `UnsupportedClass`,
    /// a class whose admission is not `BookAll` instead of leasing it a whole host.
    pub fn with_classes(mut self, classes: Vec<ResourceClass>) -> Self {
        self.classes = classes;
        self
    }

    /// The hosts that are booked and the lease holding each, read from the pool's own
    /// table.
    pub fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        let held = self
            .hosts
            .iter()
            .filter_map(|(id, host)| Some((id.clone(), host.lease?)));
        held.collect()
    }

    fn read(&self, lease: &LeaseId) -> Observed {
        let holder = self
            .hosts
            .iter()
            .find(|(_, host)| host.lease.as_ref() == Some(lease));
        match holder {
            Some((host, _)) => Observed::Present { host: host.clone() },
            None => Observed::Absent,
        }
    }
}

impl Provider for StaticPool {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn flavour(&self) -> Flavour {
        Flavour::Delegated
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.classes.clone()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        if let Observed::Present { host } = self.read(&spec.lease()) {
            return StartAck::Started { host };
        }
        if !self.classes.contains(spec.class()) {
            return StartAck::Refused(RefusalCode::UnsupportedClass);
        }
        let leases_whole_host = match spec.class().admission() {
            Admission::BookAll => true,
            Admission::Book(_) => false,
        };
        if !leases_whole_host {
            return StartAck::Refused(RefusalCode::UnsupportedClass);
        }
        let wanted = spec.request();
        let mut fits_somewhere = false;
        for (id, host) in &mut self.hosts {
            if !host
                .shape
                .satisfies(&wanted.resources, &wanted.capabilities)
            {
                continue;
            }
            fits_somewhere = true;
            if host.lease.is_none() {
                host.lease = Some(spec.lease());
                return StartAck::Started { host: id.clone() };
            }
        }
        match fits_somewhere {
            true => StartAck::Refused(RefusalCode::NoCapacity),
            false => StartAck::Refused(RefusalCode::NoFit),
        }
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        for host in self.hosts.values_mut() {
            if host.lease.as_ref() == Some(lease) {
                host.lease = None;
            }
        }
        StopAck::Stopped {
            read_back: self.read(lease),
        }
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.read(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        let listed = self.hosts.iter();
        let listed = listed.map(|(id, host)| HostCapacity {
            host: id.clone(),
            shape: host.shape.clone(),
            leased_to: host.lease,
        });
        listed.collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use js_types::{Capabilities, ClassId, Dim, Request, Resources};

    use super::*;
    use crate::config::EXAMPLE_CONFIG;

    fn shared_class() -> ResourceClass {
        let gate = BTreeSet::from([Dim::Gpus]);
        let id = ClassId::new("gpu-shared").unwrap();
        ResourceClass::new(id, Admission::Book(gate), 1)
    }

    fn spec_of(class: &ResourceClass, sequence: u64) -> StartSpec {
        let request = Request {
            class: class.id().clone(),
            resources: Resources::new(1000, 1, 1, Some("nvidia-a100".to_string())).unwrap(),
            capabilities: Capabilities::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
        };
        let known = [ResourceClass::gpu_host(), shared_class()];
        StartSpec::new(LeaseId::new(1, sequence), request, &known).unwrap()
    }

    /// Asserts that a class admitted by `Book` is refused as unsupported and books
    /// nothing, while a `BookAll` class of the same pool still gets a whole host, so a
    /// class added as data never silently receives whole-host semantics.
    #[test]
    fn a_class_that_is_not_book_all_is_refused() {
        let id = ProviderId::new("pool").unwrap();
        let pool = StaticPool::from_toml(id, EXAMPLE_CONFIG).unwrap();
        let mut pool = pool.with_classes(vec![ResourceClass::gpu_host(), shared_class()]);
        let refused = pool.start(spec_of(&shared_class(), 1));
        assert_eq!(refused, StartAck::Refused(RefusalCode::UnsupportedClass));
        assert!(pool.booked().is_empty());
        let started = pool.start(spec_of(&ResourceClass::gpu_host(), 2));
        assert!(matches!(started, StartAck::Started { .. }));
        assert_eq!(pool.booked().len(), 1);
    }
}
