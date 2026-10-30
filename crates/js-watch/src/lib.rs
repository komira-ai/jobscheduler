//! `js-watch`: a read-only watcher of a scheduler's cloud that runs apart from the lease
//! server, and the abort that stops everything the scheduler launched.
//!
//! The lease server tells an operator about leaks (its alerts), but a server that is down
//! or wedged tells nobody, exactly when machines leak. This program runs elsewhere (another
//! host, another account's credentials) and does three things:
//!
//! - [`Watcher::tick`] compares the cloud's list of the scheduler's instances with the
//!   machines the server's status page shows ([`status`]), checks that the server's tick
//!   time still moves, and alerts on a stale heartbeat or an unexplained instance;
//! - the same tick runs a periodic sweep for leftovers that bill after their instance is
//!   gone ([`CloudView::strays`]); they are counted and reported, never released;
//! - [`abort`] stops every instance of one scheduler id, a dry run unless told to apply.
//!
//! The watcher holds a [`CloudView`], a [`StatusSource`] and an alert sink. None of them has
//! a call that makes or ends anything: for Lambda the view is a `js_provider_lambdalabs::Reader`
//! over the read-only `LambdaRead`. Only [`abort`], reached from the `abort` subcommand,
//! holds a client that can terminate.
//!
//! Configuration is one TOML file ([`Config`]); a secret is the path of a file.

pub mod abort;
mod cli;
pub mod config;
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
