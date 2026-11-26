use std::collections::BTreeMap;

use js_agent_wire::{
    DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, PROTOCOL_MAJOR, ValidationError, Welcome,
    limits,
};
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT};
use js_provider::HostBook;
use js_types::{
    ACCEL_KEYS, Admission, ExecMode, HostId, MemoryHeadroom, MemoryPercent, OS_VERSION_KEY,
    ProviderId, ResourceClass, Resources, Shape,
};

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
    /// Two pools of one registry have one name ([`crate::Registry::with_pools`]).
    #[error("two pools are named {0:?}: a hello could not say which it joins")]
    PoolNamedTwice(String),
    /// A dedicated pool has no ordinary work to keep hosts for.
    #[error("keep_for_ordinary is for mixed pools; a dedicated pool keeps none")]
    DedicatedKeep,
    /// A mixed pool whose whole-node hosts and ordinary floor do not fit its size.
    #[error("max_whole_node plus keep_for_ordinary must not exceed max_machines")]
    WholeCeiling,
    /// A dedicated pool that allows more whole hosts than it has machines.
    #[error("max_whole_node must not exceed max_machines")]
    WholeAboveMachines,
    /// A pool whose capabilities name `scratch-quota` books no disk: the quota every run is
    /// held to would be booked against a shape of zero bytes, and every run refused.
    #[error("a pool that names scratch-quota must set a disk_bytes above zero")]
    QuotaWithoutDisk,
    /// The class a pool shares its hosts with books whole hosts, or is the pool's own.
    #[error("must name a class that books part of a host, other than the pool's class")]
    ShareClass,
}

/// What an operator decides about a pool of self-registering hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    name: ProviderId,
    class: ResourceClass,
    shape: Shape,
    booked: Shape,
    /// The shape a request must fit to fit the pool: `booked`, except with
    /// [`PoolConfig::booking_whole_memory`], where it keeps the memory cut.
    fit: Shape,
    memory_percent: MemoryPercent,
    /// The book takes the whole memory of each host; the percent is the placer's rule.
    whole_memory: bool,
    max_machines: usize,
    heartbeat_interval_ms: u64,
    grace_ms: u64,
    lost_retention_ms: u64,
    whole_node: Option<WholeNodeConfig>,
    exec: Option<ExecMode>,
    share_class: Option<ResourceClass>,
    /// Each host is placed by the capabilities its hello reported on top of the pool's
    /// (see [`PoolConfig::placing_by_hello`]).
    by_hello: bool,
}

impl PoolConfig {
    /// A pool called `name` of hosts of `class` with the shape `shape`, that holds at most
    /// `max_machines` hosts.
    ///
    /// A host's capacity is what it reports. A registered host is booked at the CPU, memory,
    /// GPUs and disk its last hello reported, less or more than `shape`, its memory then cut
    /// to the pool's [`memory percent`](PoolConfig::with_memory_percent) (all of it unless
    /// set); its GPUs are booked only when it can serve them ([`Self::booked_from`]). `shape`
    /// is no floor and no ceiling: its numbers are booked only for a host that has reported
    /// nothing since a restart, and its capabilities (and GPU class) are what a hello is held
    /// to. Timing starts at the wire defaults, and a lost host that holds no booking is kept
    /// for [`DEFAULT_LOST_RETENTION_MS`].
    pub fn new(name: ProviderId, class: ResourceClass, shape: Shape, max_machines: usize) -> Self {
        Self {
            name,
            class,
            booked: shape.clone(),
            fit: shape.clone(),
            shape,
            memory_percent: MemoryPercent::FULL,
            whole_memory: false,
            max_machines,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            grace_ms: DEFAULT_GRACE_MS,
            lost_retention_ms: DEFAULT_LOST_RETENTION_MS,
            whole_node: None,
            exec: None,
            share_class: None,
            by_hello: false,
        }
    }

    /// The pool, or the refusal of a disk setting that says two things at once. A pool whose
    /// capabilities name `scratch-quota` books each run without its own `disk_bytes` the
    /// host's default quota, so it must say it books disk (`disk_bytes` above zero in its
    /// shape): a zero there means the pool books no disk per job, and is also what a host
    /// that has not reported its own disk since a restart is booked. A pool that does not
    /// name `scratch-quota` may leave `disk_bytes` at zero. Every loader of an operator's pool file calls
    /// this once the pool is built; [`PoolConfig::new`] cannot, being infallible.
    pub fn checked(self) -> Result<Self, ConfigError> {
        let quota = self
            .shape
            .capabilities()
            .contains_key(js_caps::SCRATCH_QUOTA);
        match (quota, self.shape.resources().disk_bytes()) {
            (true, 0) => Err(ConfigError::QuotaWithoutDisk),
            _ => Ok(self),
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
            host_credential: None,
            dispatch_poll: false,
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

    /// The same pool requiring the host contract of `exec`: a host is admitted only if
    /// it reports `runtime` equal to `exec` (and `exec`, when it reports one, equal to it),
    /// and, when the host is booked GPUs (the pool's shape has GPUs and the host reports
    /// some), a working `gpu-toolkit` (see
    /// [`js_caps::HostContract`]). A host that does not is refused `host-contract` or
    /// `execution-mismatch` before its shape is compared.
    ///
    /// A pool made without this setting requires nothing of the kind, so hosts that
    /// predate the capabilities keep joining it.
    ///
    /// Admission makes the reported mode the pool's mode, so the pool's hosts are booked
    /// with `exec` among their capabilities (unless the shape names one): a request that
    /// requires `exec=sandbox-exec` is placed on a host that reported a `sandbox-exec`
    /// runtime, never on a container host, whatever the pool or its cloud is called. A pool
    /// without this setting books its shape as given, and a request that requires `exec`
    /// finds no host there unless the shape's capabilities name it (then admission holds
    /// each host to the value it reports).
    #[must_use]
    pub fn with_exec(mut self, exec: ExecMode) -> Self {
        self.exec = Some(exec);
        self.rebook();
        self
    }

    /// The same pool booking `percent` of the memory each host reports, rounded down (of the
    /// memory of the pool's shape for a host that has reported nothing since a restart). A
    /// job is placed and sized against the cut memory. A hello is not held to any memory.
    ///
    /// The percent is set here and nowhere else; the registry applies it to each host as
    /// it is listed.
    #[must_use]
    pub fn with_memory_percent(mut self, percent: MemoryPercent) -> Self {
        self.memory_percent = percent;
        self.rebook();
        self
    }

    /// The same pool, whose book takes the whole memory each host reports though the pool
    /// keeps its memory percent: for a
    /// placer that holds every host to the percent itself (the engine the server places
    /// with). The host listing ([`crate::Registry::hosts`]) still shows each host's shape
    /// cut to the percent beside its memory headroom; only the shape the book lists is the
    /// whole memory.
    #[must_use]
    pub fn booking_whole_memory(mut self) -> Self {
        self.whole_memory = true;
        self.rebook();
        self
    }

    /// The same pool, whose capabilities are what every host meets rather than what each
    /// host is: each host is placed on the shape it is booked at with the capabilities its last
    /// hello reported added, `arch` included, where the pool's shape does not name them
    /// (the pool's value stays where both name a key), and with the hello's
    /// `accelerator.*` facts as for any pool. A job's capabilities the pool's shape does
    /// not name are then the hosts' to meet, by what their hellos reported. For a pool whose hosts are launched from offers that may differ, such as in
    /// `arch` (an elastic pool of the server). The numbers booked are the same either way.
    /// Without it a host is placed on the pool's capabilities with only the hello's
    /// `accelerator.*` and `os_version` facts.
    #[must_use]
    pub fn placing_by_hello(mut self) -> Self {
        self.by_hello = true;
        self
    }

    /// Whether each host is placed by the capabilities its hello reported
    /// ([`PoolConfig::placing_by_hello`]).
    pub fn places_by_hello(&self) -> bool {
        self.by_hello
    }

    /// The booked shape and the fit shape from the shape, the execution mode and the
    /// memory percent.
    fn rebook(&mut self) {
        let mut capabilities = self.shape.capabilities().clone();
        if let Some(exec) = self.exec {
            capabilities
                .entry(js_types::EXEC_KEY.to_owned())
                .or_insert_with(|| exec.name().to_owned());
        }
        let resources = self.shape.resources();
        let cut = resources.with_memory_percent(self.memory_percent);
        let percent = match self.whole_memory {
            true => MemoryPercent::FULL,
            false => self.memory_percent,
        };
        let booked = resources.with_memory_percent(percent);
        self.fit = Shape::new(cut, capabilities.clone());
        self.booked = Shape::new(booked, capabilities);
    }

    /// The percent of its memory the pool books.
    pub fn memory_percent(&self) -> MemoryPercent {
        self.memory_percent
    }

    /// The memory of the pool's shape, the percent of it that is booked and what that
    /// comes to: the headroom of a host that has reported nothing since a restart, and the
    /// figure an elastic pool's status shows.
    pub fn memory_headroom(&self) -> MemoryHeadroom {
        let configured = self.shape.resources().memory_bytes();
        MemoryHeadroom::new(configured, self.memory_percent)
    }

    /// The memory headroom of a host whose last hello claimed `claim`: the memory it
    /// reports, at the pool's percent. With no claim, the pool's ([`Self::memory_headroom`]).
    pub(crate) fn headroom_of(&self, claim: Option<&Shape>) -> MemoryHeadroom {
        let configured = self.shape.resources().memory_bytes();
        let reported = claim.map_or(configured, |claim| claim.resources().memory_bytes());
        MemoryHeadroom::new(reported, self.memory_percent)
    }

    /// The resources a host whose last hello claimed `claim` is booked at: what the claim
    /// reports, its memory cut to `percent`. Its GPUs (and their class) are booked only when
    /// the host can serve them: the pool declares no execution mode, or the host reports a
    /// working `gpu-toolkit` ([`js_caps::GPU_TOOLKIT`]); a host of a pool with an execution
    /// mode that reports GPUs and no toolkit is booked none, so no GPU job is placed where
    /// its containers cannot reach the GPU (a pool whose shape has GPUs refuses such a host
    /// at its hello instead). With no claim, `base`, the shape a host that has reported
    /// nothing is booked at.
    pub(crate) fn booked_from(
        &self,
        base: &Resources,
        claim: Option<&Shape>,
        percent: MemoryPercent,
    ) -> Resources {
        let Some(claim) = claim else {
            return base.clone();
        };
        let reported = claim.resources().with_memory_percent(percent);
        let toolkit = claim
            .capabilities()
            .get(js_caps::GPU_TOOLKIT)
            .map(String::as_str);
        let serves_gpus = self.exec.is_none() || toolkit == Some(js_caps::FLAG_SET);
        match serves_gpus {
            true => reported,
            false => reported.without_gpus(),
        }
    }

    /// The percent of each host's memory the book lists: all of it with
    /// [`Self::booking_whole_memory`], else the pool's memory percent.
    pub(crate) fn book_percent(&self) -> MemoryPercent {
        match self.whole_memory {
            true => MemoryPercent::FULL,
            false => self.memory_percent,
        }
    }

    /// The execution mode the pool's hosts must run jobs in, when the pool declares one.
    pub fn exec(&self) -> Option<ExecMode> {
        self.exec
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

    /// The same pool, which also names `class`, a class that books part of a host (`Book`),
    /// beside its own class. The registry only keeps the class for the pool's configuration
    /// ([`PoolConfig::share_class`]); it books no lease with it.
    ///
    /// # Errors
    /// [`ConfigError::ShareClass`] when `class` books whole hosts or has the pool's own id.
    pub fn with_share_class(mut self, class: ResourceClass) -> Result<Self, ConfigError> {
        let books_part = matches!(class.admission(), Admission::Book(_));
        if !books_part || class.id() == self.class.id() {
            return Err(ConfigError::ShareClass);
        }
        self.share_class = Some(class);
        Ok(self)
    }

    /// The class the pool's hosts are shared with, when it has one
    /// ([`PoolConfig::with_share_class`]).
    pub fn share_class(&self) -> Option<&ResourceClass> {
        self.share_class.as_ref()
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

    /// The pool's name: what a hello names to join it, and the provider the bindings of the
    /// machines launched for it name.
    pub fn name(&self) -> &ProviderId {
        &self.name
    }

    /// The pool's `[shape]`: what it books of a host that has reported nothing since a
    /// restart, and the capabilities and GPU class every hello is held to. Its numbers are
    /// no floor a hello must meet and no ceiling on what a host is booked.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// What may be booked on a host whose last hello claimed `claim`, with its memory cut
    /// to the percent even with [`Self::booking_whole_memory`]: what the claim reports
    /// ([`Self::booked_from`]) with the pool's capabilities. With no claim, the fit shape.
    pub(crate) fn fit_of(&self, claim: Option<&Shape>) -> Shape {
        let resources = self.booked_from(self.fit.resources(), claim, self.memory_percent);
        Shape::new(resources, self.fit.capabilities().clone())
    }

    /// The shape a stored row of a host is placed on, from the shape the row `booked` and
    /// the `claim` it keeps: what the claim reports ([`Self::booked_from`], cut to the
    /// percent, as a row's shape is), or the row's own numbers when it keeps no claim, with
    /// the row's capabilities and the claim's ([`placement_shape`]).
    pub(crate) fn row_shape(&self, booked: &Shape, claim: Option<&Shape>) -> Shape {
        let resources = self.booked_from(booked.resources(), claim, self.memory_percent);
        let booked = Shape::new(resources, booked.capabilities().clone());
        placement_shape(&booked, claim, self.by_hello)
    }

    /// Lists `host` in `book`, free, as the pool books it.
    pub(crate) fn list_in(&self, book: &mut HostBook, host: HostId) {
        book.insert_host_with(host, self.booked.clone(), self.memory_headroom());
    }

    /// Lists `host` in `book` for a hello whose accepted `claim` is known. The host is
    /// booked at what it reports ([`Self::booked_from`]), with its memory at the share the
    /// book lists, and its
    /// `accelerator.*` capabilities are those of the claim, which a hello validated, and
    /// not the pool's, so placement sees the host's real family, variant facts and
    /// measured memory; in a pool that places by hello ([`Self::placing_by_hello`]) the
    /// claim's other capabilities the pool's shape does not name are added too. A host
    /// already listed and free is listed again with that shape.
    pub(crate) fn list_claimed(&self, book: &mut HostBook, host: HostId, claim: Option<&Shape>) {
        let resources = self.booked_from(self.booked.resources(), claim, self.book_percent());
        let booked = Shape::new(resources, self.booked.capabilities().clone());
        let shape = placement_shape(&booked, claim, self.by_hello);
        book.remove_free(&host);
        book.insert_host_with(host, shape, self.headroom_of(claim));
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
/// The shape a registered host is placed on: the resources it is booked at (`booked`, what
/// its claim reports: [`PoolConfig::booked_from`]) and the capabilities `booked` names (the
/// pool's), with its `accelerator.*` capabilities replaced by those of the host's validated
/// claim, so its real family, variant facts and measured memory are matched. With `by_hello`
/// ([`PoolConfig::placing_by_hello`]) the claim's other capabilities are added where
/// `booked` does not name them; where both name one, the pool's value stays. The claim's
/// `os_version` ([`OS_VERSION_KEY`]) replaces the pool's in every pool, as the accelerator
/// facts do: the pool's value is a least version the claim met when the host joined
/// (`os_version` is matched as at least), so a host is placed by the version it runs. A host
/// without a claim keeps the pool's.
pub(crate) fn placement_shape(booked: &Shape, claim: Option<&Shape>, by_hello: bool) -> Shape {
    let is_accel = |key: &String| ACCEL_KEYS.contains(&key.as_str());
    let replaced = |key: &String| is_accel(key) || key == OS_VERSION_KEY;
    let mut capabilities = booked.capabilities().clone();
    // The claim's `os_version`, when it has one, is written over the pool's below.
    capabilities.retain(|key, _| !is_accel(key));
    // Read against the pool's capabilities before any of the claim's is added.
    let added: Vec<(String, String)> = claim
        .into_iter()
        .flat_map(|claim| claim.capabilities())
        .filter(|(key, _)| replaced(key) || (by_hello && !capabilities.contains_key(*key)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    capabilities.extend(added);
    Shape::new(booked.resources().clone(), capabilities)
}

/// The quota in bytes a host enforces on a run that sets no `disk_bytes`: its
/// `scratch-default-gib` whole GiB, when it reports `scratch-quota` as `project`. `None`
/// when it does not, or when the default is absent, not a whole number, zero, or too
/// large to count in bytes (above the wire's `MAX_DISK_BYTES`): a host that says it bounds disk and not by how much is
/// treated as one that does not, so the server never books a guess.
pub fn quota_default(capabilities: &BTreeMap<String, String>) -> Option<u64> {
    let quota = capabilities
        .get(SCRATCH_QUOTA)
        .is_some_and(|value| value == SCRATCH_QUOTA_PROJECT);
    let gib = capabilities
        .get(SCRATCH_DEFAULT_GIB)
        .and_then(|value| value.parse::<u64>().ok());
    let bytes = gib.and_then(|gib| gib.checked_mul(1 << 30));
    let counts = |bytes: &u64| (1..=limits::MAX_DISK_BYTES).contains(bytes);
    bytes.filter(|bytes| [quota, counts(bytes)].into_iter().all(|ok| ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the bytes of a default: the largest whole-GiB default within the wire's
    /// `MAX_DISK_BYTES` counts and the next GiB does not, and the quota capability is
    /// needed. It catches a default accepted above the wire maximum, which a sum over
    /// 2^18 runs could overflow.
    #[test]
    fn the_default_is_bounded_by_the_wire_maximum() {
        let caps = |quota: &str, gib: &str| {
            BTreeMap::from([
                (SCRATCH_QUOTA.to_owned(), quota.to_owned()),
                (SCRATCH_DEFAULT_GIB.to_owned(), gib.to_owned()),
            ])
        };
        let top = limits::MAX_DISK_BYTES >> 30;
        assert_eq!(top, 65_535);
        assert_eq!(quota_default(&caps("project", "65535")), Some(top << 30));
        assert_eq!(quota_default(&caps("project", "65536")), None);
        assert_eq!(quota_default(&caps("project", "1")), Some(1 << 30));
        assert_eq!(quota_default(&caps("no", "1")), None);
    }
}
