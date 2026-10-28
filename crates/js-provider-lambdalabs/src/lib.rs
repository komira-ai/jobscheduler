//! The Lambda Cloud provider, read-only half: what the account's API says about instance
//! types, regional capacity and instances.
//!
//! This crate launches and terminates nothing. It holds
//!
//! - [`LambdaApi`], the three read calls (instance types with GPU facts and regions that
//!   have capacity, the list of instances, one instance), and its two implementations:
//!   [`HttpApi`], the real client, and [`FakeLambda`], the same calls over memory;
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
//! they must not run under a lock that other work waits on.

mod api;
mod error;
mod fake;
mod http;
mod key;
mod model;
mod options;
mod wire;

pub use api::LambdaApi;
pub use error::{ApiError, ErrorCode};
pub use fake::{Call, FakeLambda};
pub use http::{HttpApi, SetupError};
pub use key::{ApiKey, KeyError};
pub use model::{
    Architecture, Instance, InstanceId, InstanceTypeOffer, InvalidInstanceId, Region, Status, Tag,
};
pub use options::{GpuClass, OptionRow, OptionTable, OptionsError};
