use std::collections::{BTreeMap, BTreeSet};
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
///
/// The pool remembers every lease id it has stopped, as an exact set, and refuses a
/// start of one with `LeaseEnded`. A high-water mark per term would be smaller but
/// would also refuse a lower sequence that was never started and arrives late behind
/// a higher one that was stopped, so the pool keeps the set: 16 bytes per stopped lease
/// for the life of the process. The memory is in-process only, as the pool is.
#[derive(Debug)]
pub struct StaticPool {
    id: ProviderId,
    classes: Vec<ResourceClass>,
    hosts: BTreeMap<HostId, Host>,
    ended: BTreeSet<LeaseId>,
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
            ended: BTreeSet::new(),
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
        if self.ended.contains(&spec.lease()) {
            return StartAck::Refused(RefusalCode::LeaseEnded);
        }
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
        self.ended.insert(*lease);
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
