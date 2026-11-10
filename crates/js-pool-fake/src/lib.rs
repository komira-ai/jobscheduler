//! A fake host pool, for tests only: the hosts a test lists in a TOML file, each with its
//! shape and the memory it was listed with.
//!
//! The pool reads the file once. It books nothing and holds no lease: no work is placed
//! through it. No production code depends on this crate (it is a dev-dependency only), and
//! no flag or configuration of the server reads its file: the agentless hosts-file pool it
//! once served was removed, and the server serves only hosts that run `js-agent` and
//! register. The pool serves `gpu-host` ([`js_types::ResourceClass::gpu_host`]) unless
//! [`FakeHostPool::with_classes`] names others.
//!
//! - **Shape.** Each host is listed with the shape the file gives it, and with its memory
//!   cut to the file's `memory_schedulable_percent` (or the percent the caller passes, which
//!   replaces the file's), rounded down. The cut is made once, when the file is read.
//! - **Order.** [`FakeHostPool::hosts`] lists the hosts in byte order of their ids, each free
//!   of any lease.
//!
//! # Configuration
//!
//! [`FakeHostPool::from_config_file`] takes the path of the file. The file is TOML; an unknown key is
//! refused so a typo does not pass as a default, and a syntax or type error names the
//! line. This is the example file shipped with the crate:
//!
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![doc = concat!("```toml\n", include_str!("../example.toml"), "```")]

mod config;
mod pool;

pub use config::{ConfigError, EXAMPLE_CONFIG};
pub use pool::FakeHostPool;
