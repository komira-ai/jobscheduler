use std::collections::BTreeMap;
use std::path::Path;

use js_provider::{
    Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

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
