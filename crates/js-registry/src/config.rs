use js_agent_wire::{
    DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, PROTOCOL_MAJOR, ValidationError, Welcome,
};
use js_types::{ProviderId, ResourceClass, Shape};

/// What an operator decides about a pool of self-registering hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    name: ProviderId,
    class: ResourceClass,
    shape: Shape,
    max_machines: usize,
    heartbeat_interval_ms: u64,
    grace_ms: u64,
}

impl PoolConfig {
    /// A pool called `name` that leases `class`, holds at most `max_machines` hosts, and
    /// books every host as `shape`.
    ///
    /// `shape` is both the floor a host's claim must meet to join and the shape the
    /// pool books: a lease is placed and sized against it, never against what a host
    /// says about itself, so a host that claims more than the pool promises gains
    /// nothing. Timing starts at the wire defaults.
    pub fn new(name: ProviderId, class: ResourceClass, shape: Shape, max_machines: usize) -> Self {
        Self {
            name,
            class,
            shape,
            max_machines,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            grace_ms: DEFAULT_GRACE_MS,
        }
    }

    /// The same pool asking hosts to heartbeat every `heartbeat_interval_ms` and treating
    /// one as lost after `grace_ms` of silence. Refused with the wire's own rules for a
    /// welcome: the interval and grace are within bounds and the grace exceeds two
    /// intervals.
    pub fn with_timing(
        mut self,
        heartbeat_interval_ms: u64,
        grace_ms: u64,
    ) -> Result<Self, ValidationError> {
        let probe = Welcome {
            protocol: PROTOCOL_MAJOR,
            epoch: 0,
            heartbeat_interval_ms,
            grace_ms,
            pool: self.name.to_string(),
        };
        probe.validate()?;
        self.heartbeat_interval_ms = heartbeat_interval_ms;
        self.grace_ms = grace_ms;
        Ok(self)
    }

    pub(crate) fn name(&self) -> &ProviderId {
        &self.name
    }

    pub(crate) fn class(&self) -> &ResourceClass {
        &self.class
    }

    pub(crate) fn shape(&self) -> &Shape {
        &self.shape
    }

    pub(crate) fn max_machines(&self) -> usize {
        self.max_machines
    }

    pub(crate) fn heartbeat_interval_ms(&self) -> u64 {
        self.heartbeat_interval_ms
    }

    pub(crate) fn grace_ms(&self) -> u64 {
        self.grace_ms
    }
}
