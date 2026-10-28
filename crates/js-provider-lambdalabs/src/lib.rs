//! The Lambda Cloud provider: what the account's API says about instance types, regional
//! capacity and instances, and launching and terminating GPU instances under a cost guard.
//!
//! It holds
//!
//! - [`LambdaApi`], the calls (instance types with GPU facts and regions that have
//!   capacity, the list of instances, one instance, launch, terminate), and its two
//!   implementations: [`HttpApi`], the real client, and [`FakeLambda`], the same calls
//!   over memory;
//! - [`Elastic`], the provider-side contract of a scheduler (acquire, list, release), and
//!   [`LambdaProvider`], which implements it over a [`LambdaApi`]: ownership marks on
//!   everything it launches (tags and a name prefix; both are needed to terminate, either
//!   counts toward `max_machines`), an acquire that launches once per acquire id, a release that
//!   reads back, and the cost guard (disabled and dry-run by default, `max_machines`
//!   checked against a fresh list before each launch, a launch rate limit on an injected
//!   [`Clock`], a report-only [`Sweep`]);
//! - the typed answers ([`InstanceTypeOffer`], [`Instance`], [`Status`]) and the errors
//!   ([`ApiError`], [`ErrorCode`]), which are matched on Lambda's `code`, never on its
//!   message text;
//! - [`ApiKey`], the key read from an owner-only file and never printed;
//! - [`OptionTable`], the `[[option]]` rows of the configuration, parsed and checked.
//!
//! Nothing here has a built-in address: [`HttpApi::connect`] takes the base URL from its
//! caller, and a plain `http://` address is accepted for the loopback interface only.
//!
//! The provider is not wired to a scheduler. The calls block, each under a time limit, so
//! they must not run under a lock that other work waits on. It does not implement the
//! delivered whole-host `Provider` trait, which is called under the lease table's lock.

mod api;
mod clock;
mod config;
mod elastic;
mod error;
mod fake;
mod http;
mod key;
mod model;
mod options;
mod own;
mod provider;
mod select;
mod wire;

pub use api::LambdaApi;
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{ConfigError, ElasticConfig, Mode};
pub use elastic::{
    AcquireRequest, Acquired, Candidate, Elastic, Extra, GpuRequest, Held, Reason, Refusal,
    RefusalCode, Released,
};
pub use error::{ApiError, ErrorCode};

pub use fake::{Call, FakeLambda, TerminateEffect};
pub use http::{HttpApi, SetupError};
pub use key::{ApiKey, KeyError};
pub use model::{
    Architecture, Instance, InstanceId, InstanceTypeOffer, InvalidInstanceId, LaunchRequest,
    Region, Status, Tag,
};
pub use options::{GpuClass, OptionRow, OptionTable, OptionsError};
pub use own::{
    ACQUIRE_TAG, AcquireId, Claim, InvalidId, LabelError, MAX_TAGS, POOL_TAG, SCHEDULER_TAG,
    SchedulerId,
};
pub use provider::{LambdaProvider, Sweep};
