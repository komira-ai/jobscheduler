//! The server in process: the test and simulation seam (cargo feature `in-process`, which no
//! default feature list names and no shipped binary turns on, so nothing in the binary
//! reaches it).
//!
//! [`InProcess::start`] builds what [`crate::start_with_checks`] builds, from the same
//! [`Config`] and [`SharedClock`], by the same code, and stops before anything a listener
//! needs. It binds no socket and spawns no task: no serve loop, no tick loop, no scaler loop
//! and no check loop. Requests are answered by the routers it hands out, called as
//! `tower::Service`s ([`InProcess::client_router`], [`InProcess::agent_router`]), and time acts
//! only when the caller says [`InProcess::tick`]. It reads no environment and no clock but the
//! [`SharedClock`] it is given. What only a listener reads (the listen addresses, the TLS
//! acceptor, the connection limits, `--drain-timeout`) is not used.
//!
//! The lease table times its saves on the configuration's monotonic clock, which is the
//! system's unless [`Config::with_monotonic`] gives another: a caller that must read no real
//! clock gives it one of its own.
//!
//! A request carries no peer address, so the status routes (`/v1/pools`, `/status`) answer
//! as they do for a peer the listener could not name: as a route that does not exist.

use std::sync::Arc;

use axum::Router;
use js_lease::{Clock, Timings};
use js_registry::Registry;
use tokio::sync::Mutex;

use crate::adapt::SharedClock;
use crate::api::{Shared, agent_router, router};
use crate::checks::{Checker, Schedule};
use crate::config::Config;
use crate::error::StartupError;
use crate::scaler::Scaler;
use crate::server::{Opened, open, prepare};

/// A server built in process, with no listener and no task of its own.
///
/// Dropping it, or [`InProcess::shutdown`], drops what the server holds, the lease table
/// and with it its lock on the lease file. A router handed out holds the table too: drop
/// every clone of the routers before the handle when a second server is to open the same
/// lease file.
pub struct InProcess {
    shared: Arc<Shared>,
    scaler: Option<Scaler>,
    /// The check loop of the elastic pool with its schedule, `None` without one. Held for
    /// the length of a tick, so two ticks never run at once.
    checks: Mutex<Option<Checks>>,
}

/// The pool checks and when their next pass is due.
struct Checks {
    checker: Checker,
    schedule: Schedule,
}

impl InProcess {
    /// Builds the server `config` describes, on `clock`: the lease table over the state file
    /// (`--boot` as given), the agent side, and the elastic pool with its scaler and checks
    /// when the configuration has one. Each failure to start names the flag at fault, as
    /// [`crate::start_with_checks`] does, except that no listen address can fail.
    pub fn start(config: Config, clock: SharedClock) -> Result<Self, StartupError> {
        let (prepared, _listeners) = prepare(config, &clock)?;
        let Opened {
            shared,
            scaler,
            checker,
            ..
        } = open(prepared, &clock, |_| {})?;
        let checks = checker.map(|checker| Checks {
            checker,
            schedule: Schedule::first(clock.now()),
        });
        Ok(Self {
            shared,
            scaler,
            checks: Mutex::new(checks),
        })
    }

    /// The client, operator and run routes, behind the bearer token, with `/healthz` and the
    /// status routes open, as the listen address serves them when the agent routes have a
    /// port of their own. The agent routes answer 404 here; they are
    /// [`InProcess::agent_router`]'s.
    pub fn client_router(&self) -> Router {
        router(self.shared.clone(), false)
    }

    /// The agent routes alone (hello, heartbeat and the rest), as `server.agent_listen`
    /// serves them. Without agent registration it serves nothing.
    pub fn agent_router(&self) -> Router {
        agent_router(&self.shared)
    }

    /// One tick, its parts in this order and each run to its end: the elastic pool's checks
    /// (a pass when one is due by the clock), then the lease table and the agent registry
    /// (what the tick loop does at a tick), then the scaler. In the server the three run on
    /// three tasks, and the scaler skips a tick while it is still busy with the one before;
    /// this order is one of the interleavings those tasks allow, and the scaler is never
    /// skipped. A second call waits for the first to finish.
    pub async fn tick(&self) {
        let mut checks = self.checks.lock().await;
        if let Some(Checks { checker, schedule }) = &mut *checks {
            checker.step(schedule).await;
        }
        self.shared.tick().await;
        if let Some(scaler) = &self.scaler {
            scaler.tick().await;
        }
    }

    /// A handle on the agent registry (the same registry the routes and the tick use);
    /// `None` without agent registration. As [`crate::Running::registry`].
    pub fn registry(&self) -> Option<Registry> {
        self.shared.registry()
    }

    /// The scaler of the elastic pool of the configuration file; `None` without one. As
    /// [`crate::Running::scaler`].
    pub fn scaler(&self) -> Option<&Scaler> {
        self.scaler.as_ref()
    }

    /// How long the lease table's steps took since the server started, on the
    /// configuration's monotonic clock. As [`crate::Running::timings`].
    pub fn timings(&self) -> Timings {
        self.shared.timings()
    }

    /// How many ticks failed to write the lease file since the server started. As
    /// [`crate::Running::save_errors`].
    pub fn save_errors(&self) -> u64 {
        self.shared.save_errors()
    }

    /// How many times the agent side could not read or write one of its files since the
    /// server started; always 0 without agent registration. As
    /// [`crate::Running::agent_errors`].
    pub fn agent_errors(&self) -> u64 {
        self.shared.agent_errors()
    }

    /// Drops everything the server holds, with no last tick: what the lease file keeps is
    /// what the last tick or request wrote, as after a process that ends between two ticks.
    /// Once the routers' clones are dropped too, the lease file is unlocked and a second
    /// server may open it (`--boot resume`).
    pub fn shutdown(self) {
        drop(self);
    }
}
