//! The provider-neutral contract of a scheduler for machines it launches: acquire one,
//! list the ones it holds, release one.
//!
//! - [`CapacityProvider`] is the trait a provider of machines implements. It takes an
//!   [`AcquireRequest`] whose [`BootPayload`] is rendered by the service and delivered by
//!   the provider as it is: a provider never sees a network mode or a boot plan. Beside
//!   acquire, list and release it says who it is (a `js_types::ProviderId`), what it could ever
//!   offer ([`ProviderCaps`]), how many machines it holds and could add ([`Capacity`]),
//!   what a read of one machine by id found ([`Observed`]), which of its launches an
//!   agent's first [`Hello`] comes from, and, if it sends any, its notices
//!   ([`EventStream`] of [`ProviderEvent`]).
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
//! - `conformance` (module, behind the `conformance` feature, off by default) is the suite
//!   of cases every provider runs: `conformance::run_all` over a `conformance::Rig` the
//!   provider's crate writes. A new provider crate enables the feature in its
//!   `[dev-dependencies]` and adds one test file that calls it. A provider that cannot
//!   launch runs `conformance::run_all_registered` over a `conformance::RegisteredRig`
//!   instead.
//!
//! Nothing here does I/O or reads a clock; the lease-level `Pool` of `js-provider` (the
//! scheduler's allocation port) is a different trait.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod answer;
mod boot;
mod caps;
#[cfg(feature = "conformance")]
pub mod conformance;
mod ids;
mod offer;
mod provider;
mod read;
mod secret;

pub use answer::{
    Acquired, Claim, Extra, Held, HostStatus, Observed, Reason, Refusal, RefusalCode, Released,
};
pub use boot::{
    Artifact, BootPlan, BootStep, CONTAINER_BIN, RUNTIME, SCRATCH_ROOT, STATE_ROOT, SlotName,
};
pub use caps::{Capacity, ProviderCaps};
pub use ids::{AcquireId, Hello, HostName, HostRef, InvalidId, SchedulerId};
pub use offer::{Demand, Dimension, Offer, PriceUnit};
pub use provider::{
    AcquireRequest, CapacityProvider, CheckResult, EventStream, ProviderError, ProviderEvent,
};
pub use read::ReadHandle;
pub use secret::{BootPayload, Secret};
