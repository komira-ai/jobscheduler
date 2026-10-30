//! What the scaler is told about a pool: its settings, whether its checks pass, and why a
//! launch is held back.

use std::time::Duration;

use crate::time::Millis;

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
    /// How long a launched machine may take to say hello, counted from the launch. A
    /// machine is given up on at this time only if the registry has also not shown its host
    /// on complete views for a third of it (`boot_hold`); see the design, section 6.2.
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
    /// The longest a machine lives, counted from its launch. A registered host that
    /// reaches it is drained and then released whether or not it is idle: the drain stops
    /// new leases and adds no deadline of its own, so the work on the host runs to its end
    /// (a lease ends within the server's ceiling on `max_wall`) before the release. `None`
    /// for a pool whose machines are bounded by idleness alone; the server's configuration
    /// requires it of a pool with accelerators.
    pub max_age: Option<Duration>,
    /// The least a machine lives before `idle` may drain it, counted from its launch. It
    /// bounds the churn of a short `idle` against a billed boot: every machine launched
    /// serves at least this long. It does not hold back a drain for `max_age` or for a
    /// host that came up below its offer.
    pub min_host_lifetime: Duration,
    /// The server's ceiling on a lease's `max_wall`, if it has one. With `max_age` it
    /// gives [`Settings::lifetime`].
    pub max_lease_wall: Option<Duration>,
    /// How long a whole-node lease may wait, pending, before its wall time starts (the
    /// registry's prepare timeout): a lease booked just before the retire can end this much
    /// later than the ceiling alone says. Part of [`Settings::lifetime`].
    pub prepare_timeout: Duration,
    /// What [`Settings::lifetime`] adds beyond `max_age`, the ceiling, `prepare_timeout` and
    /// `unknown_hold`, for the scaler's ticks, the drain and release round trips and the
    /// provider's clock.
    pub lifetime_margin: Duration,
}

/// The default of [`Settings::lifetime_margin`]: 10 minutes.
const LIFETIME_MARGIN: Duration = Duration::from_secs(10 * 60);

/// The default of [`Settings::prepare_timeout`]: 10 minutes, the registry's default.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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
            max_age: None,
            min_host_lifetime: Duration::ZERO,
            max_lease_wall: None,
            prepare_timeout: PREPARE_TIMEOUT,
            lifetime_margin: LIFETIME_MARGIN,
        }
    }
}

impl Settings {
    /// The lifetime the cloud itself enforces on every machine of the pool, so that billing
    /// stops even if this scaler is dead. It covers the worst case of a retire at `max_age`:
    /// a whole-node lease booked just before it starts its wall time up to `prepare_timeout`
    /// later and lasts the ceiling on `max_wall`; a host that went silent is released only
    /// after `unknown_hold`; and `lifetime_margin` is left for ticks and round trips. So it
    /// is `max_age + max_lease_wall + prepare_timeout + unknown_hold + lifetime_margin`. A provider with a native limit sets it from this
    /// at launch (a Compute Engine `maxRunDuration`, a shutdown timer); one without (Lambda)
    /// ignores it. `None` unless both `max_age` and the ceiling are set.
    #[must_use]
    pub fn lifetime(&self) -> Option<Duration> {
        let age = self.max_age?;
        let wall = self.max_lease_wall?;
        let waits = self.prepare_timeout.saturating_add(self.unknown_hold);
        let after = wall
            .saturating_add(waits)
            .saturating_add(self.lifetime_margin);
        Some(age.saturating_add(after))
    }

    /// Whether a machine launched at `launched` has reached `max_age` at `now`.
    pub(crate) fn aged(&self, launched: Millis, now: Millis) -> bool {
        self.max_age.is_some_and(|max| now.since(launched) >= max)
    }

    /// How long the join token of a launch lives: `boot_timeout` plus `token_margin`.
    pub(crate) fn token_life(&self) -> Duration {
        self.boot_timeout.saturating_add(self.token_margin)
    }

    /// How long a booting machine past `boot_timeout` must have been missing from complete
    /// registry views before it is given up on: a third of `boot_timeout` (5 minutes by
    /// default), and never more than `unknown_hold`.
    ///
    /// The registry keeps no host in memory across a restart; an agent that was waiting to
    /// retry says hello again within its longest wait (two minutes) plus the time the
    /// registry takes to admit it, so a machine launched before the restart can be past
    /// `boot_timeout` at the very first complete view and be seconds from its hello. The hold
    /// is longer than that wait and shorter than `boot_timeout`, so in steady state, where
    /// the count has run since the launch, the machine is still given up on at
    /// `boot_timeout`.
    pub(crate) fn boot_hold(&self) -> Duration {
        (self.boot_timeout / 3).min(self.unknown_hold)
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
    /// An offer of the catalog would serve a waiting request but has no price, or a price
    /// of zero, so it is never launched. The catalog needs fixing; nothing else will unblock
    /// the request.
    Unpriced,
    /// The pool holds `max_instances`.
    MaxInstances,
    /// One more machine would take the hourly spend over `max_spend_per_hour`.
    MaxSpend,
    /// One more machine would take the day's spend over `max_spend_per_day`.
    MaxSpendDay,
}
