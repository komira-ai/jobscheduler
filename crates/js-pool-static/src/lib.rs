//! A static pool: the hosts an operator lists in a TOML file, as the server shows them.
//!
//! The pool reads the file once and lists each host with its shape and the memory it was
//! listed with. It books nothing and holds no lease: no work is placed through it, it has
//! no lease file, and a restart loses nothing. The server (`js-lease-server`) lists these
//! hosts beside the hosts that register themselves, and refuses a hello for any of their
//! ids. The pool serves `gpu-host` ([`js_types::ResourceClass::gpu_host`]) unless
//! [`StaticPool::with_classes`] names others.
//!
//! - **Shape.** Each host is listed with the shape the file gives it, and with its memory
//!   cut to the file's `memory_schedulable_percent` (or the percent the caller passes, which
//!   replaces the file's), rounded down. The cut is made once, when the file is read.
//! - **Order.** [`StaticPool::hosts`] lists the hosts in byte order of their ids, each free
//!   of any lease.
//!
//! # Configuration
//!
//! [`StaticPool::from_config_file`] takes the path of the file, which the caller
//! reads from its own `--pool-config` style flag. The file is TOML; an unknown key is
//! refused so a typo does not pass as a default, and a syntax or type error names the
//! line. This is the example file shipped with the crate:
//!
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![doc = concat!("```toml\n", include_str!("../example.toml"), "```")]

mod config;
pub mod kind;
mod pool;

pub use config::{ConfigError, EXAMPLE_CONFIG};
pub use pool::StaticPool;
