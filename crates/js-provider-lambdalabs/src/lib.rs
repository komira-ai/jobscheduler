//! The Lambda Cloud provider: what the account's API says about instance types, regional
//! capacity and instances, and launching and terminating GPU instances under a cost guard.
//!
//! It holds
//!
//! - [`LambdaApi`], the calls (instance types with GPU facts and regions that have
//!   capacity, the list of instances, one instance, launch, terminate), and its two
//!   implementations: [`HttpApi`], the real client, and [`FakeLambda`], the same calls
//!   over memory;
//! - [`LambdaProvider`], which implements the scheduler's provider-neutral contract
//!   ([`js_elastic::ElasticProvider`]: catalog, host preparation, acquire, list, release) over a
//!   [`LambdaApi`]: ownership marks on
//!   everything it launches (tags and a name prefix; both are needed to terminate, either
//!   counts toward `max_machines`), an acquire that launches once per acquire id, a release that
//!   reads back, and the cost guard (disabled and dry-run by default, a launch rate limit
//!   on an injected [`Clock`], a report-only [`Sweep`], and `max_machines`: a hard cap on
//!   the instances carrying our markers, counted from a fresh list before each launch;
//!   it is this provider's own backstop, not a pool-level cost policy);
//! - the typed answers ([`InstanceTypeOffer`], [`Instance`]) and the errors
//!   ([`ApiError`], [`ErrorCode`]), which are matched on Lambda's `code`, never on its
//!   message text;
//! - [`ApiKey`], the key read from an owner-only file and never printed;
//! - [`OptionTable`], the `[[option]]` rows of the configuration, parsed and checked.
//!
//! Nothing here has a built-in address: [`HttpApi::connect`] takes the base URL from its
//! caller, and a plain `http://` address is accepted for the loopback interface only.
//!
//! The guard's memory (launches sent, launches answered and not yet listed, the time of
//! the last launch) is one process's `Mutex`: run one provider instance per scheduler id.
//! A restart, or a second process, sees only the list, which can lag a write.
//!
//! A release that answers [`js_elastic::Released::Unknown`] means "not known": callers must
//! retry it, never read it as "gone".
//!
//! A launch carries, besides the type, the region, the name and the ownership tags:
//!
//! - the boot payload the service rendered, as `user_data`, delivered as it is and never
//!   printed (it holds the machine's join token): a [`js_elastic::Secret`] in the request,
//!   with no `Clone` or `PartialEq`, left out of every `Debug`, log line and error;
//!   [`PAYLOAD_LIMIT`] is the size the provider accepts, and a larger payload is refused
//!   before any call;
//! - the id of a firewall ruleset named `js-<scheduler>-noinbound` that holds no rule, found
//!   or created once per region ([`LambdaProvider::ensure_firewall`]) and attached to every
//!   launch, because a ruleset can only be attached at launch. A launch without one is
//!   refused as a shape error and never sent;
//! - one ssh key name, which the API requires and nothing uses: the configured name, else
//!   the account's key `js-<scheduler>`, else its only key, else a key made under that
//!   name ([`LambdaProvider::ssh_key`], [`ElasticConfig::with_account_key`]).
//!
//! A dry run builds a launch and calls nothing, so it makes no ruleset and no key.
//! [`HttpApi`] trusts the public roots bundled in the program for `https` unless it is
//! given a CA bundle, which then replaces them.
//!
//! [`js_elastic::ElasticProvider::host_prep`] returns the steps that prepare Lambda's
//! stock image (the driver and container program checks, and a scratch file system of its
//! own under the agent's scratch root, sized by [`ElasticConfig::scratch_gib`]).
//!
//! The provider is not wired to a scheduler. The calls block, each under a time limit, so
//! they must not run under a lock that other work waits on. It does not implement the
//! delivered whole-host `Provider` trait, which is called under the lease table's lock.

mod api;
mod clock;
mod config;
mod error;
mod fake;
pub mod first_launch;
mod host;
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
pub use config::{
    ConfigError, DEFAULT_SCRATCH_GIB, ElasticConfig, MAX_SCRATCH_GIB, MIN_SCRATCH_GIB, Mode,
};
pub use error::{ApiError, ErrorCode};

pub use fake::{Call, FakeLambda, LaunchRecord, TerminateEffect};
pub use host::{DISK_RESERVE_GIB, SCRATCH_IMAGE};
pub use http::{HttpApi, SetupError};
pub use key::{ApiKey, KeyError};
pub use model::{
    Architecture, FirewallRuleset, Instance, InstanceId, InstanceTypeOffer, InvalidInstanceId,
    LaunchRequest, Region, SshKey, Tag,
};
pub use options::{OptionRow, OptionTable, OptionsError};
pub use own::{ACQUIRE_TAG, LabelError, MAX_TAGS, POOL_TAG, SCHEDULER_TAG};
pub use provider::{LambdaProvider, PAYLOAD_LIMIT, PrepareError, Sweep};
