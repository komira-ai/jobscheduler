use js_agent_wire::{
    DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, PROTOCOL_MAJOR, ValidationError, Welcome,
};
use js_provider::HostBook;
use js_types::{
    ACCEL_KEYS, Admission, ExecMode, HostId, MemoryHeadroom, MemoryPercent, ProviderId,
    ResourceClass, Shape,
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
    /// The book takes the whole memory of `shape`; the percent is the placer's rule.
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
    /// A pool called `name` that leases `class`, holds at most `max_machines` hosts, and
    /// books every host as `shape`.
    ///
    /// `shape` is both the floor a host's claim must meet to join and, cut to the pool's
    /// [`memory percent`](PoolConfig::with_memory_percent) (all of it unless set), the
    /// shape the pool books: a lease is placed and sized against the booked shape, never
    /// against what a host says about itself, so a host that claims more than the pool
    /// promises gains nothing. Its `disk_bytes` is both floor and booking, uncut. Timing
    /// starts at the wire defaults, and a lost host that holds no
    /// booking is kept for [`DEFAULT_LOST_RETENTION_MS`].
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

    /// The pool, or the refusal of a disk setting that would admit hosts and then refuse
    /// every run. A pool whose capabilities name `scratch-quota` must book disk
    /// (`disk_bytes` above zero in its shape), because a run without its own `disk_bytes`
    /// is booked the host's default quota, and a pool that books none could never hold
    /// one. A pool that does not name `scratch-quota` may leave `disk_bytes` at zero,
    /// which books no disk for any run. Every loader of an operator's pool file calls
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
    /// and, in a pool whose shape has GPUs, a working `gpu-toolkit` (see
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

    /// The same pool booking `percent` of the memory of its shape, rounded down, for
    /// every host: a lease, a whole-node lease, a run and a reservation are all placed and
    /// sized against the cut memory. A host's hello is still held to the whole shape, so a
    /// host must really have the memory the pool lists; the pool books less of it.
    ///
    /// The cut is made here and nowhere else, on the memory the operator configured, and
    /// never on what a host claims.
    #[must_use]
    pub fn with_memory_percent(mut self, percent: MemoryPercent) -> Self {
        self.memory_percent = percent;
        self.rebook();
        self
    }

    /// The same pool, whose book takes the whole memory of `[shape]` though the pool keeps
    /// its memory percent: for a placer that holds every host to the percent itself
    /// (`--placement js-sched` of the lease server). The percent still decides what fits
    /// the pool, so a request above the share is refused `NoFit`, and the host listing
    /// ([`crate::Registry::hosts`]) still shows each host's shape cut to it beside the
    /// memory headroom ([`PoolConfig::memory_headroom`]); only the room the book checks,
    /// and the capacity it lists to the lease table, are the whole memory.
    #[must_use]
    pub fn booking_whole_memory(mut self) -> Self {
        self.whole_memory = true;
        self.rebook();
        self
    }

    /// The same pool, whose shape is a floor every host meets rather than what each host
    /// is: each host is placed on the pool's booked shape with the capabilities its last
    /// hello reported added, `arch` included, where the pool's shape does not name them
    /// (the pool's value stays where both name a key), and with the hello's
    /// `accelerator.*` facts as for any pool. A request then fits the pool when the pool's
    /// shape fits its resources and the capabilities the pool's shape names
    /// (`accelerator.*` included); the capabilities it does not name are the hosts' to
    /// meet. For a pool whose hosts are launched from offers that may differ, such as in
    /// `arch` (an elastic pool of the lease server). The numbers booked are still the
    /// pool's. Without it a host is placed on the pool's shape with only the hello's
    /// `accelerator.*` facts, and a request must fit the pool's shape whole.
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

    /// The memory the pool lists for a host, the percent of it that is booked and what
    /// that comes to.
    pub fn memory_headroom(&self) -> MemoryHeadroom {
        let configured = self.shape.resources().memory_bytes();
        MemoryHeadroom::new(configured, self.memory_percent)
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

    /// The same pool, whose hosts also take leases of `class`, a class that books part of
    /// a host (`Book`), beside its own class: leases of `class` share a host that has room,
    /// placed by a placer ([`crate::Registry::set_placer`]; without one every such lease
    /// is refused as an unsupported class). Its provider ([`crate::Registry::provider`])
    /// serves both classes.
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

    /// The shape every host of the pool is booked as: [`Self::shape`], with its memory cut
    /// to the pool's memory percent (not with [`Self::booking_whole_memory`]) and the pool's
    /// execution mode when it declares one.
    pub(crate) fn booked(&self) -> &Shape {
        &self.booked
    }

    /// The shape a request must fit to fit the pool: [`Self::booked`], with its memory cut
    /// to the percent even with [`Self::booking_whole_memory`].
    pub(crate) fn fit(&self) -> &Shape {
        &self.fit
    }

    /// Lists `host` in `book`, free, as the pool books it.
    pub(crate) fn list_in(&self, book: &mut HostBook, host: HostId) {
        book.insert_host_with(host, self.booked.clone(), self.memory_headroom());
    }

    /// Lists `host` in `book` for a hello whose accepted `claim` is known. The host books
    /// as [`Self::booked`] does (the pool's numbers are the booking limits), but its
    /// `accelerator.*` capabilities are those of the claim, which a hello validated, and
    /// not the pool floor's, so placement sees the host's real family, variant facts and
    /// measured memory; in a pool that places by hello ([`Self::placing_by_hello`]) the
    /// claim's other capabilities the pool's shape does not name are added too. A host
    /// already listed and free is listed again with that shape. One holding a lease keeps
    /// the shape it was booked with, except in a pool that places by hello, where it takes
    /// the new shape with its leases (the numbers are the pool's either way): a host put
    /// back with a lease after a restart, listed with the pool's shape alone, is then
    /// placed by its hello as soon as it says one.
    pub(crate) fn list_claimed(&self, book: &mut HostBook, host: HostId, claim: Option<&Shape>) {
        let shape = placement_shape(&self.booked, claim, self.by_hello);
        book.remove_free(&host);
        if self.by_hello {
            book.reshape(&host, shape.clone());
        }
        book.insert_host_with(host, shape, self.memory_headroom());
    }

    /// Whether `request` fits the pool: [`Self::fit`] fits its resources and capabilities.
    /// In a pool that places by hello ([`Self::placing_by_hello`]) only the capabilities
    /// the fit shape names count, with the `accelerator.*` ones: the others are the hosts'
    /// to meet, by what their hellos reported.
    pub(crate) fn fits(&self, request: &js_types::Request) -> bool {
        let fit = &self.fit;
        let named = |key: &String| {
            ACCEL_KEYS.contains(&key.as_str()) || fit.capabilities().contains_key(key)
        };
        let mut wanted = request.capabilities.clone();
        if self.by_hello {
            wanted.retain(|key, _| named(key));
        }
        fit.satisfies(&request.resources, &wanted)
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
/// The shape a registered host is placed on: the resources and capabilities the pool books
/// (`booked`), with its `accelerator.*` capabilities replaced by those of the host's
/// validated claim, so its real family, variant facts and measured memory are matched while
/// the pool's numbers stay the booking limits. With `by_hello`
/// ([`PoolConfig::placing_by_hello`]) the claim's other capabilities are added where
/// `booked` does not name them; where both name one, the pool's value stays. A host without
/// a claim keeps the pool's.
pub(crate) fn placement_shape(booked: &Shape, claim: Option<&Shape>, by_hello: bool) -> Shape {
    let is_accel = |key: &String| ACCEL_KEYS.contains(&key.as_str());
    let mut capabilities = booked.capabilities().clone();
    capabilities.retain(|key, _| !is_accel(key));
    // Read against the pool's capabilities before any of the claim's is added.
    let added: Vec<(String, String)> = claim
        .into_iter()
        .flat_map(|claim| claim.capabilities())
        .filter(|(key, _)| is_accel(key) || (by_hello && !capabilities.contains_key(*key)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    capabilities.extend(added);
    Shape::new(booked.resources().clone(), capabilities)
}
