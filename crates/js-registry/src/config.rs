use js_agent_wire::{
    DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, PROTOCOL_MAJOR, ValidationError, Welcome,
};
use js_types::{Admission, ProviderId, ResourceClass, Shape};

use crate::WholeNodeConfig;

/// How long a lost host that holds no booking is kept, unless the operator says
/// otherwise: one day.
pub const DEFAULT_LOST_RETENTION_MS: u64 = 86_400_000;
/// The shortest retention [`PoolConfig::with_lost_retention`] accepts: one minute.
pub const MIN_LOST_RETENTION_MS: u64 = 60_000;
/// The longest retention [`PoolConfig::with_lost_retention`] accepts: thirty days.
pub const MAX_LOST_RETENTION_MS: u64 = 30 * 86_400_000;

/// Why a pool setting was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The lost-host retention is outside [`MIN_LOST_RETENTION_MS`] to
    /// [`MAX_LOST_RETENTION_MS`].
    #[error("the lost-host retention must be between one minute and thirty days")]
    LostRetention,
    /// The pool's class is not a whole-node class or a mixed one, so whole-node
    /// settings mean nothing on it.
    #[error("whole-node settings need a class that offers the whole-node lease")]
    WholeNodeClass,
    /// A setting that is a maximum, a count or a time is zero where zero is meaningless.
    #[error("{0} must be at least 1")]
    WholeZero(&'static str),
    /// A dedicated pool has no ordinary work to keep hosts for.
    #[error("keep_for_ordinary is for mixed pools; a dedicated pool keeps none")]
    DedicatedKeep,
    /// A mixed pool whose whole-node hosts and ordinary floor do not fit its size.
    #[error("max_whole_node plus keep_for_ordinary must not exceed max_machines")]
    WholeCeiling,
    /// A dedicated pool that allows more whole hosts than it has machines.
    #[error("max_whole_node must not exceed max_machines")]
    WholeAboveMachines,
}

/// What an operator decides about a pool of self-registering hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    name: ProviderId,
    class: ResourceClass,
    shape: Shape,
    max_machines: usize,
    heartbeat_interval_ms: u64,
    grace_ms: u64,
    lost_retention_ms: u64,
    whole_node: Option<WholeNodeConfig>,
}

impl PoolConfig {
    /// A pool called `name` that leases `class`, holds at most `max_machines` hosts, and
    /// books every host as `shape`.
    ///
    /// `shape` is both the floor a host's claim must meet to join and the shape the
    /// pool books: a lease is placed and sized against it, never against what a host
    /// says about itself, so a host that claims more than the pool promises gains
    /// nothing. Timing starts at the wire defaults, and a lost host that holds no
    /// booking is kept for [`DEFAULT_LOST_RETENTION_MS`].
    pub fn new(name: ProviderId, class: ResourceClass, shape: Shape, max_machines: usize) -> Self {
        Self {
            name,
            class,
            shape,
            max_machines,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            grace_ms: DEFAULT_GRACE_MS,
            lost_retention_ms: DEFAULT_LOST_RETENTION_MS,
            whole_node: None,
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
            session: "probe".to_string(),
        };
        probe.validate()?;
        self.heartbeat_interval_ms = heartbeat_interval_ms;
        self.grace_ms = grace_ms;
        Ok(self)
    }

    /// The same pool keeping a lost host that holds no booking for `retention_ms` after
    /// the host became lost, then forgetting it together with the credential and
    /// instance it was bound to. A host that holds a booking is never forgotten.
    ///
    /// Refused outside [`MIN_LOST_RETENTION_MS`] to [`MAX_LOST_RETENTION_MS`], so that a
    /// slip neither forgets hosts the moment they blink nor keeps dead ids for ever.
    pub fn with_lost_retention(mut self, retention_ms: u64) -> Result<Self, ConfigError> {
        let allowed = MIN_LOST_RETENTION_MS..=MAX_LOST_RETENTION_MS;
        match allowed.contains(&retention_ms) {
            true => {
                self.lost_retention_ms = retention_ms;
                Ok(self)
            }
            false => Err(ConfigError::LostRetention),
        }
    }

    /// The same pool with the whole-node settings `whole`, checked against the pool: its
    /// class must be `Whole` (a dedicated pool) or `Book` (a mixed one); every maximum,
    /// count and time except `drain_after_ms` is at least 1; a dedicated pool keeps no
    /// host for ordinary work and has `max_whole_node <= max_machines`; a mixed pool has
    /// `max_whole_node + keep_for_ordinary <= max_machines`.
    pub fn with_whole_node(mut self, whole: WholeNodeConfig) -> Result<Self, ConfigError> {
        let mixed = match self.class.admission() {
            Admission::Whole => false,
            Admission::Book(_) => true,
            Admission::BookAll => return Err(ConfigError::WholeNodeClass),
        };
        if let Some(key) = whole.zero_key() {
            return Err(ConfigError::WholeZero(key));
        }
        match (mixed, whole.keep_for_ordinary) {
            (false, 1..) => return Err(ConfigError::DedicatedKeep),
            (false, _) if whole.max_whole_node > self.max_machines => {
                return Err(ConfigError::WholeAboveMachines);
            }
            (true, _) if whole.max_whole_node + whole.keep_for_ordinary > self.max_machines => {
                return Err(ConfigError::WholeCeiling);
            }
            _ => {}
        }
        self.whole_node = Some(whole);
        Ok(self)
    }

    /// The whole-node settings, when the pool has them.
    pub fn whole_node(&self) -> Option<&WholeNodeConfig> {
        self.whole_node.as_ref()
    }

    /// How many hosts of a dedicated pool can never be whole because `max_whole_node`
    /// is below `max_machines`; they serve nothing. Zero for a pool without settings.
    pub fn whole_surplus(&self) -> usize {
        let whole = self.whole_node.as_ref();
        whole.map_or(0, |w| self.max_machines.saturating_sub(w.max_whole_node))
    }

    pub(crate) fn name(&self) -> &ProviderId {
        &self.name
    }

    pub(crate) fn class(&self) -> &ResourceClass {
        &self.class
    }

    /// The shape the pool books for every host of it.
    pub fn shape(&self) -> &Shape {
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

    pub(crate) fn lost_retention_ms(&self) -> u64 {
        self.lost_retention_ms
    }
}
