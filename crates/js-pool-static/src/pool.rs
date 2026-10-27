use std::collections::BTreeMap;
use std::path::Path;

use js_provider::{Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

use crate::book::{HostBook, SeatError};
use crate::config::{self, ConfigError};

/// A provider over a fixed list of hosts, each leased whole. See the crate docs for
/// the placement rules, which live in [`HostBook`].
///
/// The pool remembers every lease id it has stopped (see [`HostBook`]), in memory only,
/// as the pool is.
#[derive(Debug)]
pub struct StaticPool {
    id: ProviderId,
    classes: Vec<ResourceClass>,
    book: HostBook,
}

impl StaticPool {
    /// A pool over configuration text.
    pub fn from_toml(id: ProviderId, source: &str) -> Result<Self, ConfigError> {
        let hosts = config::parse(source)?;
        let mut book = HostBook::new();
        for (host, shape) in hosts {
            book.insert_host(host, shape);
        }
        Ok(Self {
            id,
            classes: vec![ResourceClass::gpu_host()],
            book,
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
    /// a class whose admission does not book a whole host (`BookAll` or `Whole`) instead
    /// of leasing it a whole host.
    pub fn with_classes(mut self, classes: Vec<ResourceClass>) -> Self {
        self.classes = classes;
        self
    }

    /// The hosts that are booked and the lease holding each, read from the pool's own
    /// table.
    pub fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.book.booked()
    }

    /// Books `host` for `lease` without a start: the way to put back a booking the
    /// pool lost with its process, when the lease file still records it. The booking is
    /// what a start would have made, so the lease then observes `Present` on `host`
    /// and a stop frees it. Seating a lease on the host it already holds changes
    /// nothing.
    ///
    /// The pool refuses, and books nothing, when `host` is not listed, when another
    /// lease holds it, when `lease` holds another host, or when `lease` was stopped.
    pub fn seat(&mut self, host: &HostId, lease: LeaseId) -> Result<(), SeatError> {
        self.book.seat(host, lease)
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
        let gate = HostBook::class_gate(&self.classes, &spec);
        self.book.start_where(&spec, gate, &|_, _| true)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        StopAck::Stopped {
            read_back: self.book.stop(lease),
        }
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.book.read(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.book.hosts()
    }
}
