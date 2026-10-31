//! The provider-neutral contract of a scheduler for machines it launches: acquire one,
//! list the ones it holds, release one.
//!
//! - [`ElasticProvider`] is the trait a provider implements. It takes an
//!   [`AcquireRequest`] whose [`BootPayload`] is rendered by the service and delivered by
//!   the provider as it is: a provider never sees a network mode or a boot plan.
//! - [`ReadHandle`] is a provider seen through its reads only: code that holds one cannot
//!   acquire or release, which is how a pool in plan mode is kept from spending.
//! - [`Offer`] is what one launchable thing would give, and [`Offer::fit`] says whether
//!   it serves a [`Demand`] (an alias of `js_types::Requirements`), naming the
//!   [`Dimension`] it fails.
//! - [`Acquired`], [`Released`] and [`Held`] are the answers. A release that answers
//!   [`Released::Unknown`] means "not known": callers must retry it, never read it as
//!   "gone".
//! - [`Secret`] holds text that must not be printed: no `Display`, no `Clone`, and a
//!   `Debug` that prints `***`.
//! - [`BootPlan`] is a launch's boot steps as data, with secrets as named slots.
//! - [`SchedulerId`], [`AcquireId`], [`HostName`] and [`HostRef`] are the ids the two
//!   sides share. A [`HostName`] is derived from the scheduler and the acquire, and
//!   starts with a prefix only that scheduler's hosts carry.
//!
//! Nothing here does I/O or reads a clock; the lease-level `Provider` of `js-provider`
//! is a different trait and does not change.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod answer;
mod boot;
mod ids;
mod offer;
mod provider;
mod read;
mod secret;

pub use answer::{
    Acquired, Claim, Extra, Held, HostStatus, Reason, Refusal, RefusalCode, Released,
};
pub use boot::{
    Artifact, BootPlan, BootStep, CONTAINER_BIN, RUNTIME, SCRATCH_ROOT, STATE_ROOT, SlotName,
};
pub use ids::{AcquireId, HostName, HostRef, InvalidId, SchedulerId};
pub use offer::{Demand, Dimension, GpuClass, Offer, PriceUnit};
pub use provider::{AcquireRequest, CheckResult, ElasticProvider, ProviderError};
pub use read::ReadHandle;
pub use secret::{BootPayload, Secret};
