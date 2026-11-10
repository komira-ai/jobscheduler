//! Elastic pools: a pool whose machines the server's scaler launches on demand, on an
//! [`js_elastic_fake::ElasticFake`] the cell owns, and that boot into the cell's agents.
//!
//! An [`ElasticPool`] names the cloud's billing, its offers, its `max_instances` and the
//! pool keys the server's configuration takes; [`crate::ServerSpec::elastic_pool`] makes
//! a server of it. The server node then reads its configuration from a file
//! (`Config::from_config_file`) with one provider kind of the cell's (`sim-cloud`) that
//! hands the scaler the pool's [`Cloud`]: the fake behind a recording wrapper, which
//! forwards every call and writes down each acquire, release and list with its arguments, its
//! answer and the virtual time of its step ([`CloudCall`]). A kind is a plain function,
//! so the node leaves its clouds in a thread-local slot just before the file is read and
//! the kind takes one from there, on the same thread: that is the only read of the slot.
//! A server of [`crate::ServerSpec::ordered`] has one cloud for each of its launching
//! machine providers ([`Cloud::ordered`]), each table of the kind taking the cloud its key
//! `cloud` names.
//! The scaler draws its acquire ids, and so its machines' names, from the seed
//! ([`SeededEntropy`], through `Config::with_entropy`).
//!
//! # One clock
//!
//! The fake's clock starts at [`crate::EPOCH_MS`], virtual time zero, and the server node
//! advances it to the virtual time of every input before the server takes it, as it sets
//! the server's own clock: the fake's clock only advances, so the node moves it by the
//! difference ([`Cloud::advance_to`]).
//!
//! # A launch becomes a boot
//!
//! After every step the cell reads the machines the fake has launched. For each, it reads
//! the launch payload through the fake's own accessor, `ElasticFake::user_data`, the boot
//! script the server rendered, exactly as the server's tests read it: the one-time join
//! token from the line that writes the token file, and the host id from the agent's
//! `--host-id` ([`Payload`]). Nothing reaches the agent by another way. The pool's boot
//! time (the fake's [`js_elastic_fake::Knobs::boot`]) later, of virtual time, the cell adds
//! an agent node for the machine, unless the cloud no longer runs it or the pool does not
//! join ([`ElasticPool::joins`]): its bearer is that token, its host id that name, its
//! hello names the instance `<pool>:<name>` that the token is bound to, and it announces
//! the server's shape with the capabilities of the boot script's agent (a container
//! runtime and the GPU toolkit). The node is named `m000`, `m001`, ... in its cloud's
//! launch order, after the provider's name and a dash for a cloud of
//! [`crate::ServerSpec::ordered`] (`lambda-m000`).
//! A machine the fake preempts, drops or releases has its agent killed in the same step:
//! its host is gone, not a fault.
//!
//! # What is not modelled
//!
//! The wrapper sends no reclaim notices (`CapacityProvider::events` answers `None`): the
//! server reads such a stream on a thread of its own, which would act at a moment of real
//! time, not of the seed's. A preempted machine is found by the scaler's next list.

use std::time::Duration;

use js_elastic_fake::{Billing, Knobs};

mod cloud;
pub(crate) mod config;
mod entropy;
mod facts;
pub(crate) mod payload;
mod record;

pub use cloud::{Cloud, offer};
pub use entropy::SeededEntropy;
pub use facts::{CloudFacts, Machine};
pub use payload::Payload;
pub use record::{AcquireAnswer, AcquireSeen, CloudCall, CloudEvent, EarlierSeen, ReleaseAnswer};

/// How long a host of an elastic pool stays quiet before the scaler drains and releases
/// it, unless [`ElasticPool::idle`] says otherwise (the pool key `idle`).
pub const DEFAULT_IDLE: Duration = Duration::from_secs(60);

/// How long the scaler keeps a machine at the least from its launch (the pool key
/// `min_host_lifetime`).
pub const MIN_HOST_LIFETIME: Duration = Duration::from_secs(60);

/// The age at which the scaler retires a machine (the pool key `max_age`).
pub const MAX_AGE: Duration = Duration::from_secs(2 * 3_600);

/// How long pass 2 passes over an instance type in a region after a `quota` refusal, unless
/// [`ElasticPool::failover`] says otherwise: the server's default `quota_backoff`.
pub const DEFAULT_QUOTA_BACKOFF: Duration = Duration::from_secs(3_600);

/// How long a request every provider refused waits before the first is asked again, unless
/// [`ElasticPool::failover`] says otherwise: the server's default `acquire_retry`.
pub const DEFAULT_ACQUIRE_RETRY: Duration = Duration::from_secs(60);

/// One offer of an elastic pool's cloud: its id, its zone and its price. Every offer is
/// a machine of the server's shape ([`crate::Shape`]), of the `linux` and `x86_64` kind,
/// that runs containers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferSpec {
    /// The offer's id.
    pub id: String,
    /// The offer's zone.
    pub zone: Option<String>,
    /// Its price, in cents an hour.
    pub cents_per_hour: u64,
}

impl OfferSpec {
    /// The offer `id` in `zone` at `cents_per_hour`.
    #[must_use]
    pub fn new(id: &str, zone: &str, cents_per_hour: u64) -> Self {
        Self {
            id: id.to_owned(),
            zone: Some(zone.to_owned()),
            cents_per_hour,
        }
    }
}

/// A pool whose machines the server's scaler launches on an elastic fake cloud: how the
/// cloud bills, what it offers, how many of the pool's machines may run at once, and the
/// pool keys of the server's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElasticPool {
    name: String,
    billing: Billing,
    offers: Vec<OfferSpec>,
    max_instances: u64,
    idle: Duration,
    knobs: Knobs,
    joins: bool,
    quota_backoff: Duration,
    acquire_retry: Duration,
}

impl ElasticPool {
    /// A pool `name` on a cloud that bills as EC2 does (per second, at least a minute:
    /// [`Billing::Aws`]), offering `offers`, of at most `max_instances` machines.
    #[must_use]
    pub fn ec2_like(name: &str, offers: Vec<OfferSpec>, max_instances: u64) -> Self {
        Self::billed(name, Billing::Aws, offers, max_instances)
    }

    /// A pool `name` on a cloud that bills as Lambda does (each started minute whole, at
    /// least one: [`Billing::Lambda`]), offering `offers`, of at most `max_instances`
    /// machines.
    #[must_use]
    pub fn lambda_like(name: &str, offers: Vec<OfferSpec>, max_instances: u64) -> Self {
        Self::billed(name, Billing::Lambda, offers, max_instances)
    }

    fn billed(name: &str, billing: Billing, offers: Vec<OfferSpec>, max_instances: u64) -> Self {
        Self {
            name: name.to_owned(),
            billing,
            offers,
            max_instances,
            idle: DEFAULT_IDLE,
            knobs: Knobs::default(),
            joins: true,
            quota_backoff: DEFAULT_QUOTA_BACKOFF,
            acquire_retry: DEFAULT_ACQUIRE_RETRY,
        }
    }

    /// This pool, as one machine provider of a server of [`crate::ServerSpec::ordered`],
    /// with the `provider_limits` `quota_backoff` and `acquire_retry` of the server's pool on
    /// it. A server of [`crate::ServerSpec::elastic_pool`] writes neither, and takes the
    /// server's defaults ([`DEFAULT_QUOTA_BACKOFF`], [`DEFAULT_ACQUIRE_RETRY`]).
    #[must_use]
    pub fn failover(mut self, quota_backoff: Duration, acquire_retry: Duration) -> Self {
        self.quota_backoff = quota_backoff;
        self.acquire_retry = acquire_retry;
        self
    }

    /// This pool, whose quiet hosts are drained and released after `idle`.
    #[must_use]
    pub fn idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// This pool, on a cloud of `knobs` (its quota, room, boot time and lags).
    #[must_use]
    pub fn knobs(mut self, knobs: Knobs) -> Self {
        self.knobs = knobs;
        self
    }

    /// This pool, whose booted machines say hello (`true`, the default) or never do
    /// (`false`: the cell boots no agent for them).
    #[must_use]
    pub fn joins(mut self, joins: bool) -> Self {
        self.joins = joins;
        self
    }

    /// The pool's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How the cloud bills.
    #[must_use]
    pub fn billing(&self) -> Billing {
        self.billing
    }

    /// The cloud's offers.
    #[must_use]
    pub fn offers(&self) -> &[OfferSpec] {
        &self.offers
    }

    /// The most machines of the pool that may run at once.
    #[must_use]
    pub fn max_instances(&self) -> u64 {
        self.max_instances
    }

    /// How long a host stays quiet before it is drained and released.
    #[must_use]
    pub fn idle_time(&self) -> Duration {
        self.idle
    }

    /// The cloud's knobs.
    #[must_use]
    pub fn cloud_knobs(&self) -> &Knobs {
        &self.knobs
    }

    /// Whether booted machines say hello.
    #[must_use]
    pub fn joined(&self) -> bool {
        self.joins
    }

    /// How long an instance type in a region is passed over after a `quota` refusal.
    #[must_use]
    pub fn quota_backoff(&self) -> Duration {
        self.quota_backoff
    }

    /// How long a request every provider refused waits before the first is asked again.
    #[must_use]
    pub fn acquire_retry(&self) -> Duration {
        self.acquire_retry
    }
}
