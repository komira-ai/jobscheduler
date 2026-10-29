//! What the scaler is told about a pool: its settings, whether its checks pass, and why a
//! launch is held back.

use std::time::Duration;

/// Whether a pool may spend money (`launch` in the pool's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// The pool checks and shows what it would launch, and launches nothing.
    Plan,
    /// The pool launches.
    Live,
}

/// What the pool's background checks say. Only `Available` lets a launch through; a
/// pool in any state still scales down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The first full check pass has not finished.
    Checking,
    /// The checks pass.
    Available,
    /// A check fails.
    Failing,
}

/// The settings of one elastic pool. Money is in the smallest part of the pool's price
/// unit (US cents), as offers carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The pool's name. It becomes part of each acquire id, so it must be a valid id
    /// (1 to 64 letters, digits, `.`, `_` or `-`); a pool whose name is not launches
    /// nothing and shows [`Blocked::PoolName`].
    pub pool: String,
    /// Whether the pool launches.
    pub launch: LaunchMode,
    /// The most machines the pool may hold at once: records that count, orphans included.
    pub max_instances: usize,
    /// The most the held machines and one more may cost per hour.
    pub max_spend_per_hour: u64,
    /// The most the machines launched in the last 24 hours and one more may cost, over
    /// their billed time.
    pub max_spend_per_day: u64,
    /// How long a registered host holds no lease and no run before it is drained.
    pub idle: Duration,
    /// How long a launched machine may take to say hello, counted from the launch.
    pub boot_timeout: Duration,
    /// How long past `boot_timeout` a join token lives, so a host that boots at the very
    /// limit still finds its token alive.
    pub token_margin: Duration,
    /// How long the pool launches nothing after a boot failure, a quota or a refusal.
    pub boot_fail_cooldown: Duration,
    /// How long an unanswered or unseen acquire is waited for before it is given up on,
    /// and the least gap between the two clean lists that settle it.
    pub unknown_hold: Duration,
    /// The most launches one step makes.
    pub max_step: usize,
}

impl Settings {
    /// A pool in `Plan` mode with the design's defaults: `idle` 10 minutes, `boot_timeout`
    /// 15 minutes, `token_margin` 5 minutes (a token lives 20), `boot_fail_cooldown` 10
    /// minutes, `unknown_hold` 30 minutes and `max_step` 1.
    #[must_use]
    pub fn new(
        pool: &str,
        max_instances: usize,
        max_spend_per_hour: u64,
        max_spend_per_day: u64,
    ) -> Self {
        Self {
            pool: pool.to_owned(),
            launch: LaunchMode::Plan,
            max_instances,
            max_spend_per_hour,
            max_spend_per_day,
            idle: Duration::from_secs(10 * 60),
            boot_timeout: Duration::from_secs(15 * 60),
            token_margin: Duration::from_secs(5 * 60),
            boot_fail_cooldown: Duration::from_secs(10 * 60),
            unknown_hold: Duration::from_secs(30 * 60),
            max_step: 1,
        }
    }
}

impl Settings {
    /// How long the join token of a launch lives: `boot_timeout` plus `token_margin`.
    pub(crate) fn token_life(&self) -> Duration {
        self.boot_timeout.saturating_add(self.token_margin)
    }
}

/// Why a pool launches nothing now, as the pool's status shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// The pool's first check pass has not finished.
    Checking,
    /// A check fails.
    Unavailable,
    /// The pool is in `Plan` mode.
    Plan,
    /// The pool's name is not a valid id, so no acquire id can carry it.
    PoolName,
    /// The list of held machines failed, so what exists is not known.
    ListFailed,
    /// A boot failed lately.
    BootFailed,
    /// A provider answered that the account's limit was reached.
    Quota,
    /// A provider answered that the request was not made.
    Denied,
    /// A provider answered that the request does not fit anything it has.
    Shape,
    /// A launch the provider told to wait is waiting, so nothing new is launched until its
    /// time.
    Deferred,
    /// The caller gave fewer random values than launches, or repeated one.
    Entropy,
    /// The pool holds `max_instances`.
    MaxInstances,
    /// One more machine would take the hourly spend over `max_spend_per_hour`.
    MaxSpend,
    /// One more machine would take the day's spend over `max_spend_per_day`.
    MaxSpendDay,
}
