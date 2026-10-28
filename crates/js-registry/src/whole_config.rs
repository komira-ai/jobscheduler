//! The pool settings of the whole-node class (design whole-node.md, section 6 and the
//! timing keys of sections 3 to 5): plain data with the defaults of the design's table.
//! Nothing here reads a clock or a file; [`crate::PoolConfig::with_whole_node`] checks a
//! value against its pool.
//!
//! A pool file's values are loaded into this type and put on the registry's pool. The
//! registry reads the caps, `drain_after`, `drain_deadline`, `reserve_backoff` and
//! `max_reserve_attempts` of its reservations (`whole::reserve`), `prepare_timeout` and
//! `max_replace` (the prepare failure handling, and how long a request's record is
//! kept) and the dedicated defaults for a pool that has none; the lease server also
//! hands `prepare_timeout` to its lease table.

/// How long a host stays excluded for a request after a give-up or a failed prepare:
/// five minutes.
pub const DEFAULT_RESERVE_BACKOFF_MS: u64 = 300_000;
/// The default time a lease may take to become ready, counted from its commit: ten
/// minutes.
pub const DEFAULT_PREPARE_TIMEOUT_MS: u64 = 600_000;
/// The default time after a boot in which an unclaimed reservation record is kept: the
/// wire's heartbeat grace, since hosts re-announce themselves within it.
pub const DEFAULT_REGAIN_GRACE_MS: u64 = js_agent_wire::DEFAULT_GRACE_MS;

/// What an operator decides about the whole-node class of one pool. Times are
/// milliseconds. The fields are public so a file's overrides read as struct update on a
/// default; a value is checked when it is put on a pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeNodeConfig {
    /// Most hosts whole (reserved plus held) at once.
    pub max_whole_node: usize,
    /// Most hosts reserved and not yet held at once.
    pub max_draining: usize,
    /// Most hosts one consumer has whole at once; `None` is no limit (the merged
    /// service has one bearer token, so every client is one consumer).
    pub max_whole_node_per_consumer: Option<usize>,
    /// Fewest hosts of a mixed pool left neither reserved nor held.
    pub keep_for_ordinary: usize,
    /// Whether a reservation may start on the last capable host of an older ordinary
    /// request.
    pub allow_last_capable: bool,
    /// How long a request waits before it reserves; zero reserves at once.
    pub drain_after_ms: u64,
    /// The reservation deadline; `None` is the pool's max wall time at use time.
    pub drain_deadline_ms: Option<u64>,
    /// How long a host stays excluded for a request after a give-up or a failed prepare.
    pub reserve_backoff_ms: u64,
    /// Give-ups after which a reservation's deadline is the max wall time.
    pub max_reserve_attempts: u32,
    /// The time a lease may take to become ready, counted from its commit.
    pub prepare_timeout_ms: u64,
    /// Prepare failures after which a request fails `exclusive-not-ready`.
    pub max_replace: u32,
    /// How long after a boot an unclaimed reservation record is kept.
    pub regain_grace_ms: u64,
}

impl WholeNodeConfig {
    /// The defaults of a dedicated pool of `max_machines` hosts: every host may be
    /// whole, none is kept for ordinary work.
    pub fn dedicated(max_machines: usize) -> Self {
        Self::with_ceiling(max_machines, 0)
    }

    /// The defaults of a mixed pool that gives up to `max_whole_node` hosts, keeping
    /// one for ordinary work.
    pub fn mixed(max_whole_node: usize) -> Self {
        Self::with_ceiling(max_whole_node, 1)
    }

    fn with_ceiling(max_whole_node: usize, keep_for_ordinary: usize) -> Self {
        Self {
            max_whole_node,
            max_draining: 1,
            max_whole_node_per_consumer: None,
            keep_for_ordinary,
            allow_last_capable: false,
            drain_after_ms: 0,
            drain_deadline_ms: None,
            reserve_backoff_ms: DEFAULT_RESERVE_BACKOFF_MS,
            max_reserve_attempts: 2,
            prepare_timeout_ms: DEFAULT_PREPARE_TIMEOUT_MS,
            max_replace: 3,
            regain_grace_ms: DEFAULT_REGAIN_GRACE_MS,
        }
    }

    /// The reservation deadline for a pool whose max wall time is `max_wall_ms`.
    pub fn drain_deadline_ms(&self, max_wall_ms: u64) -> u64 {
        self.drain_deadline_ms.unwrap_or(max_wall_ms)
    }

    /// The most hosts one consumer may have whole at once.
    pub fn per_consumer_limit(&self) -> usize {
        self.max_whole_node_per_consumer
            .unwrap_or(self.max_whole_node)
    }

    /// The first setting that is zero where zero is meaningless, by key name.
    pub(crate) fn zero_key(&self) -> Option<&'static str> {
        let counts = [
            ("max_whole_node", self.max_whole_node as u64),
            ("max_draining", self.max_draining as u64),
            (
                "max_whole_node_per_consumer",
                self.per_consumer_limit() as u64,
            ),
            ("max_reserve_attempts", u64::from(self.max_reserve_attempts)),
            ("max_replace", u64::from(self.max_replace)),
            ("reserve_backoff", self.reserve_backoff_ms),
            ("prepare_timeout", self.prepare_timeout_ms),
            ("regain_grace", self.regain_grace_ms),
            ("drain_deadline", self.drain_deadline_ms(1)),
        ];
        counts
            .into_iter()
            .find(|(_, value)| *value == 0)
            .map(|(key, _)| key)
    }
}
