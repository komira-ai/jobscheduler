//! What the scaler is told about a pool: its settings, whether its checks pass, and why a
//! launch is held back.

use std::time::Duration;

use js_types::ProviderId;

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
    /// How long the pool launches nothing after a boot failure or a `denied` or `shape`
    /// refusal.
    pub boot_fail_cooldown: Duration,
    /// The longest that cooldown grows to: it doubles with each failure in a row, from
    /// `boot_fail_cooldown`, and stops here (never below `boot_fail_cooldown` itself). The server's file allows 10
    /// minutes to 24 hours.
    pub boot_fail_cooldown_max: Duration,
    /// How long a machine's host must be heard from, unbroken, before the machine ends the
    /// count of failures in a row. A host that says hello and then goes quiet never does. The
    /// server's file allows 1 minute to 24 hours.
    pub boot_fail_reset_after: Duration,
    /// How many failures in a row trip the pool to `Plan` until an operator resets it; at
    /// least 1 (a smaller figure is read as 1).
    pub boot_fail_trip_after: u32,
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
    /// serves at least this long. It does not hold back a drain for `max_age`, nor the
    /// release of a machine whose hello did not match its offer.
    pub min_host_lifetime: Duration,
    /// When the pool's go expires, if it does. At and after this instant the pool is in
    /// `Plan` mode whatever `launch` says ([`Settings::mode`]), so a forgotten `launch =
    /// "live"` stops spending money by itself. `None` for a go that does not expire.
    pub live_until: Option<Millis>,
    /// The provider kind of the pool, such as `lambdalabs`: the key of the totals that
    /// apply to every pool of the kind ([`Budgets::kinds`](crate::Budgets::kinds)). Empty
    /// for a pool in no kind.
    pub kind: String,
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
    /// The pool's first machine provider, as pass 2 of placement names it
    /// (`js_sched::acquire`). The server gives the provider's id; [`UNNAMED_PROVIDER`]
    /// until it does. This and the fields after it, to `offer_cooldown`, are the first
    /// provider's; [`Settings::later`] holds the providers after it.
    pub provider: ProviderId,
    /// The provider's share of the pool: the most machines it may hold for the pool at
    /// once (its `provider_limits` `max_machines`, else the pool's). `None`: no share of
    /// its own. A launch that would pass it is not made, and the pool shows
    /// [`Blocked::MaxInstances`], as for `max_instances`.
    pub provider_share: Option<u32>,
    /// The instance types (offer ids) the pool may launch, from the provider's
    /// `provider_limits`. `None`: any.
    pub allowed_instance_types: Option<Vec<String>>,
    /// The highest price per hour, in cents, the pool may launch at, from the provider's
    /// `provider_limits`; an offer at exactly this price is inside. `None`: any price.
    pub max_price_cents_per_hour: Option<u64>,
    /// How long, after the provider refused an acquire for `quota`, pass 2 passes over that
    /// instance type in that region (its `provider_limits` `quota_backoff`; design 5.6,
    /// step 5). [`QUOTA_BACKOFF`] by default.
    pub quota_backoff: Duration,
    /// How long a request waits before pass 2 asks again once every offer was refused or
    /// passed over for it (its `provider_limits` `acquire_retry`). A completed catalog
    /// refresh of the pool ([`crate::PoolInput::catalog_refreshed`]) ends the wait at once.
    /// [`ACQUIRE_RETRY`] by default.
    pub acquire_retry: Duration,
    /// How far below the offer's CPU, memory and disk, in percent of each, the shape a
    /// machine's hello announces may be and still match the offer (its `provider_limits`
    /// `size_tolerance_percent`; design 5.6.1, step 6): an agent measures a little less than
    /// the nominal size. The server's file allows 0 to 50. [`SIZE_TOLERANCE_PERCENT`] by
    /// default.
    pub size_tolerance_percent: u8,
    /// How long an offer is cooled down, keyed by (provider, instance type, zone), after the
    /// hello of a machine acquired for it did not match it (its `provider_limits`
    /// `offer_cooldown`). [`OFFER_COOLDOWN`] by default.
    pub offer_cooldown: Duration,
    /// The pool's machine providers after the first, in the order pass 2 walks them
    /// (design 5.6, step 5): a provider is asked for a launch only when every provider
    /// before it has no usable offer, holds its share or refused every offer it had in the
    /// request's attempt. Empty by default: the pool has one provider.
    pub later: Vec<ProviderSettings>,
}

/// One machine provider of a pool after its first ([`Settings::later`]), with the
/// `provider_limits` the pool gives it. Each field means what the field of the same name
/// means for the first provider in [`Settings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSettings {
    /// The provider, as pass 2 names it.
    pub provider: ProviderId,
    /// Its provider kind, such as `lambdalabs`: the key of the totals of its kind.
    pub kind: String,
    /// Its share of the pool. `None`: no share of its own.
    pub share: Option<u32>,
    /// The instance types it may launch for the pool. `None`: any.
    pub allowed_instance_types: Option<Vec<String>>,
    /// The highest price per hour, in cents, it may launch at for the pool. `None`: any.
    pub max_price_cents_per_hour: Option<u64>,
    /// How long one of its instance types is passed over after a `quota` refusal.
    pub quota_backoff: Duration,
    /// How far below its offer a hello may be, in percent.
    pub size_tolerance_percent: u8,
    /// How long one of its offers cools down after a hello that did not match it.
    pub offer_cooldown: Duration,
}

impl ProviderSettings {
    /// The provider `provider` of kind `kind` with no share, no limits and the defaults of
    /// [`Settings::new`] for its timings and tolerance.
    #[must_use]
    pub fn new(provider: ProviderId, kind: &str) -> Self {
        Self {
            provider,
            kind: kind.to_owned(),
            share: None,
            allowed_instance_types: None,
            max_price_cents_per_hour: None,
            quota_backoff: QUOTA_BACKOFF,
            size_tolerance_percent: SIZE_TOLERANCE_PERCENT,
            offer_cooldown: OFFER_COOLDOWN,
        }
    }
}

/// The default of [`Settings::provider`], for settings no server has named a provider in.
pub const UNNAMED_PROVIDER: ProviderId = ProviderId::known("unnamed");

/// The default of [`Settings::quota_backoff`]: one hour.
pub const QUOTA_BACKOFF: Duration = Duration::from_secs(3600);

/// The default of [`Settings::acquire_retry`]: 60 seconds.
pub const ACQUIRE_RETRY: Duration = Duration::from_secs(60);

/// The default of [`Settings::size_tolerance_percent`]: 10 percent.
pub const SIZE_TOLERANCE_PERCENT: u8 = 10;

/// The default of [`Settings::offer_cooldown`]: one hour.
pub const OFFER_COOLDOWN: Duration = Duration::from_secs(3600);

/// The default of [`Settings::lifetime_margin`]: 10 minutes.
const LIFETIME_MARGIN: Duration = Duration::from_secs(10 * 60);

/// The default of [`Settings::prepare_timeout`]: 10 minutes, the registry's default.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The default of [`Settings::boot_fail_cooldown_max`]: 2 hours.
pub const BOOT_FAIL_COOLDOWN_MAX: Duration = Duration::from_secs(2 * 3600);

/// The default of [`Settings::boot_fail_reset_after`]: 10 minutes.
pub const BOOT_FAIL_RESET_AFTER: Duration = Duration::from_secs(10 * 60);

/// The default of [`Settings::boot_fail_trip_after`]: 3 failures in a row.
pub const BOOT_FAIL_TRIP_AFTER: u32 = 3;

impl Settings {
    /// A pool in `Plan` mode with the design's defaults: `idle` 10 minutes, `boot_timeout`
    /// 15 minutes, `token_margin` 5 minutes (a token lives 20), `boot_fail_cooldown` 10
    /// minutes, `unknown_hold` 30 minutes, `max_step` 1, `quota_backoff` one hour,
    /// `acquire_retry` 60 seconds, `size_tolerance_percent` 10 and `offer_cooldown` one
    /// hour; [`UNNAMED_PROVIDER`], with no
    /// share and no `provider_limits` on its offers.
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
            boot_fail_cooldown_max: BOOT_FAIL_COOLDOWN_MAX,
            boot_fail_trip_after: BOOT_FAIL_TRIP_AFTER,
            boot_fail_reset_after: BOOT_FAIL_RESET_AFTER,
            unknown_hold: Duration::from_secs(30 * 60),
            max_step: 1,
            max_age: None,
            live_until: None,
            kind: String::new(),
            min_host_lifetime: Duration::ZERO,
            max_lease_wall: None,
            prepare_timeout: PREPARE_TIMEOUT,
            lifetime_margin: LIFETIME_MARGIN,
            provider: UNNAMED_PROVIDER,
            provider_share: None,
            allowed_instance_types: None,
            max_price_cents_per_hour: None,
            quota_backoff: QUOTA_BACKOFF,
            acquire_retry: ACQUIRE_RETRY,
            size_tolerance_percent: SIZE_TOLERANCE_PERCENT,
            offer_cooldown: OFFER_COOLDOWN,
            later: Vec::new(),
        }
    }

    /// The pool's machine providers, first to last: the first from the fields of these
    /// settings, then [`Settings::later`].
    #[must_use]
    pub fn providers(&self) -> Vec<ProviderSettings> {
        let first = ProviderSettings {
            provider: self.provider.clone(),
            kind: self.kind.clone(),
            share: self.provider_share,
            allowed_instance_types: self.allowed_instance_types.clone(),
            max_price_cents_per_hour: self.max_price_cents_per_hour,
            quota_backoff: self.quota_backoff,
            size_tolerance_percent: self.size_tolerance_percent,
            offer_cooldown: self.offer_cooldown,
        };
        std::iter::once(first)
            .chain(self.later.iter().cloned())
            .collect()
    }

    /// The pool's machine provider `provider`, or its first when it has none of that id:
    /// a record written before the pool had several providers names none, and is the
    /// first's.
    pub(crate) fn provider_of(&self, provider: Option<&ProviderId>) -> ProviderSettings {
        let mut providers = self.providers();
        let at = providers
            .iter()
            .position(|each| Some(&each.provider) == provider)
            .unwrap_or(0);
        providers.swap_remove(at)
    }

    /// Whether the pool has more than one machine provider, so that its records name theirs.
    pub(crate) fn several(&self) -> bool {
        !self.later.is_empty()
    }
}

impl Settings {
    /// The lifetime the cloud itself enforces on every machine of the pool, so that billing
    /// stops even if this scaler is dead. It covers the worst case of a retire at `max_age`:
    /// a whole-node lease booked just before it starts its wall time up to `prepare_timeout`
    /// later and lasts the ceiling on `max_wall`; a host that went silent is released only
    /// after `unknown_hold`; and `lifetime_margin` is left for ticks and round trips. So it
    /// is `max_age + max_lease_wall + prepare_timeout + unknown_hold + lifetime_margin`.
    ///
    /// Still outside the sum, and so to fit in `lifetime_margin`: the lease grace and regain
    /// grace before a silent host's lease is given up, the release retry loop (it has no
    /// deadline), the gap between `launched_at` and the machine's creation, and one tick; and
    /// after a registry restart, until the next tick, an aged host is bookable again (the
    /// registry keeps no marks), which one tick plus the margin bounds. One case has no bound
    /// of its own: a run the host keeps listing after its lease ended keeps the host from
    /// being quiet, so `release_host` is refused with no deadline. A cloud that enforces the
    /// lifetime cuts it there; on Lambda it lasts until an operator acts. A provider with a native limit sets it from this
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

    /// The mode the pool is in at `now`: `launch`, except that a pool whose `live_until` has
    /// come is in `Plan`. The step and [`launchable`](crate::launchable) ask this and never read
    /// `launch` alone.
    #[must_use]
    pub fn mode(&self, now: Millis) -> LaunchMode {
        match self.live_until {
            Some(until) if now >= until => LaunchMode::Plan,
            _ => self.launch,
        }
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
    /// The failure brake tripped the pool to `Plan` after too many failures in a row, and an
    /// operator has not reset it.
    Tripped,
    /// The pool's name is not a valid id, so no acquire id can carry it.
    PoolName,
    /// The list of held machines failed, so what exists is not known.
    ListFailed,
    /// A boot failed lately.
    BootFailed,
    /// A provider answered that the account's limit was reached: shown for a cooldown an
    /// earlier release wrote. This release passes the instance type over instead.
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
    /// The pool holds `max_instances`, or every provider that could take the launch holds
    /// its share ([`Settings::provider_share`], [`ProviderSettings::share`]).
    MaxInstances,
    /// One more machine would take the hourly spend over `max_spend_per_hour`.
    MaxSpend,
    /// One more machine would take the day's spend over `max_spend_per_day`.
    MaxSpendDay,
    /// One more machine would take the spend of all the pools, or of all the pools of the
    /// provider kind, over a total ([`Totals`](crate::Totals)): the day, the month or
    /// the time since the go.
    MaxSpendTotal,
}

impl Blocked {
    /// The name `/v1/pools` shows for a cap: `Some` for the caps, `None` for the other
    /// reasons.
    #[must_use]
    pub fn cap_name(self) -> Option<&'static str> {
        match self {
            Self::MaxInstances => Some("max_instances"),
            Self::MaxSpend => Some("max_spend"),
            Self::MaxSpendDay => Some("max_spend_day"),
            Self::MaxSpendTotal => Some("max_spend_total"),
            _ => None,
        }
    }

    /// The name `/v1/pools` shows for a reason it reports: a cap's name, or `brake` for a pool
    /// the failure brake tripped; `None` for the others.
    #[must_use]
    pub fn shown_name(self) -> Option<&'static str> {
        match self {
            Self::Tripped => Some("brake"),
            other => other.cap_name(),
        }
    }
}
