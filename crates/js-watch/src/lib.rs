//! `js-watch`: a read-only watcher of a scheduler's cloud that runs apart from the lease
//! server, and the abort that stops everything the scheduler launched.
//!
//! The lease server tells an operator about leaks (its alerts), but a server that is down
//! or wedged tells nobody, exactly when machines leak. This program runs apart from it
//! (another host is the point) and does three things:
//!
//! - [`Watcher::tick`] compares the cloud's list of the scheduler's instances with the
//!   machines the server's status page shows ([`status`]), checks that the server's tick
//!   time still moves, and alerts on a stale heartbeat or an unexplained instance;
//! - the same tick runs a periodic sweep for leftovers that bill after their instance is
//!   gone ([`CloudView::strays`]); they are counted and reported, never released;
//! - [`abort`] stops every instance of one scheduler id, a dry run unless told to apply.
//!
//! The watcher reads the same Lambda API key file as the server, and that key can launch and
//! terminate: nothing about the key is read-only. The limit is by type. The watcher holds a
//! [`CloudView`], a [`StatusSource`] and an alert sink, none of which has a call that makes
//! or ends anything; for Lambda the view is a `js_provider_lambdalabs::Reader` over the
//! read-only `LambdaRead`, and the full client reaches only the `abort` subcommand. A
//! credential that can only read, kept apart from the server's, would make it true of the
//! key as well.
//!
//! Configuration is one TOML file ([`Config`]); a secret is the path of a file.

pub mod abort;
mod cli;
mod client;
pub mod config;
#[doc(hidden)]
pub mod fake;
pub mod status;
pub mod view;
pub mod watcher;

pub use abort::{Doomed, LambdaTarget, Outcome, Plan, Target, TargetError, abort};
pub use cli::{main, run};
pub use config::{Config, ConfigError};
pub use status::{HttpStatus, Status, StatusError, StatusSource};
pub use view::{CloudView, LambdaView, Seen, Stray, StrayKind, ViewError};
pub use watcher::{Report, Settings, Watcher};
