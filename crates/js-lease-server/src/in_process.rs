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
//! The metrics time what they measure on the configuration's monotonic clock, which is the
//! system's unless [`Config::with_monotonic`] gives another: a caller that must read no real
//! clock gives it one of its own.
//!
//! The scaler draws the random part of each acquire id, and so of each machine's name, from
//! the operating system unless [`Config::with_entropy`] gives another source: a caller that
//! must name the same machines on every run with the same inputs gives it a seeded one.
//!
//! A request carries no peer address, so the status routes (`/v1/pools`, `/status`) answer
//! as they do for a peer the listener could not name: as a route that does not exist.
//!
//! When the configuration turns the job service on (`--job-listen`, whose address is not
//! bound here), [`InProcess::job_router`] is the router its port serves (gRPC and the
//! HTTP/JSON gateway behind the bearer token), and [`InProcess::jobs`] the job and allocation
//! tables it serves from, for a caller that checks them. The tables stamp their rows with the
//! clock and mint their ids from the source [`Config::with_job_sources`] gave, the system's
//! otherwise.

use std::sync::Arc;

use axum::Router;
use js_jobs::JobStore;
use js_registry::Registry;
use js_state::Store;
use js_types::Clock;
use tokio::sync::Mutex;

use crate::api::{Shared, agent_router, router};
use crate::catalogs::Cached;
use crate::checks::{Checker, Schedule};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::error::StartupError;
use crate::job_api;
pub use crate::job_store::ServerIds;
use crate::scaler::Scaler;
use crate::server::{Opened, open, prepare};

/// A server built in process, with no listener and no task of its own.
///
/// Dropping it, or [`InProcess::shutdown`], drops what the server holds. A router handed out
/// holds the server's state too: drop every clone of the routers before the handle when a
/// second server is to open the same state files.
pub struct InProcess {
    shared: Arc<Shared>,
    scaler: Option<Scaler>,
    /// The cache of the launching providers' catalogs, fetched at each tick before the
    /// scaler's.
    cached: Cached,
    /// The check loop of the elastic pool with its schedule, `None` without one. Held for
    /// the length of a tick, so two ticks never run at once.
    checks: Mutex<Option<Checks>>,
    /// The router of the job service's port, `None` while the job service is off.
    job: Option<Router>,
}

/// The pool checks and when their next pass is due.
struct Checks {
    checker: Checker,
    schedule: Schedule,
}

impl InProcess {
    /// Builds the server `config` describes, on `clock`: the agent side, and the elastic pool
    /// with its scaler and checks when the configuration has one. Each failure to start names the flag at fault, as
    /// [`crate::start_with_checks`] does, except that no listen address can fail.
    pub fn start(config: Config, clock: SharedClock) -> Result<Self, StartupError> {
        let log_reads = job_api::logs::Ctx::from_config(config.logs(), &clock);
        let (prepared, listeners) = prepare(config, &clock)?;
        let Opened {
            shared,
            scaler,
            checker,
            cached,
            ..
        } = open(prepared, &clock)?;
        let checks = checker.map(|checker| Checks {
            checker,
            schedule: Schedule::first(clock.now()),
        });
        let job = job_api::from_server(&shared, listeners.max_job_wall, log_reads)
            .map(|api| job_api::port(api, listeners.clients));
        Ok(Self {
            shared,
            scaler,
            cached,
            checks: Mutex::new(checks),
            job,
        })
    }

    /// The host listing, join, brake and usage routes, behind the bearer token, with
    /// `/healthz` and the status routes open, as the listen address serves them when the
    /// agent routes have a port of their own. The agent routes answer 404 here; they are
    /// [`InProcess::agent_router`]'s.
    pub fn client_router(&self) -> Router {
        router(self.shared.clone(), false)
    }

    /// The agent routes alone (hello, heartbeat and the rest), as `server.agent_listen`
    /// serves them. Without agent registration it serves nothing.
    pub fn agent_router(&self) -> Router {
        agent_router(&self.shared)
    }

    /// The job service as the port of `--job-listen` serves it: gRPC for a request whose
    /// content type is `application/grpc`, the HTTP/JSON gateway (`/v1/jobs`, ...) for any
    /// other, both behind the bearer layer of the job service's clients (the token of
    /// `--token-file` among them). `None` while the job service is off. A clone
    /// holds the job tables: drop it with the other routers before a second server opens
    /// the same files.
    pub fn job_router(&self) -> Option<Router> {
        self.job.clone()
    }

    /// The job and allocation tables the job service and the agents' per-job heartbeats are
    /// served from, to read; `None` while the job service is off.
    pub fn jobs(&self) -> Option<&JobStore<Arc<dyn Store>, ServerIds>> {
        let agents = self.shared.agents()?;
        agents.jobs.as_deref()
    }

    /// One tick, its parts in this order and each run to its end: the elastic pool's checks
    /// (a pass when one is due by the clock), then the job pass and the agent registry (what
    /// the tick loop does at a tick), then the fetch of the catalogs that are due, then the
    /// scaler. In the server the checks, the tick and the fetch-then-scaler run
    /// on three tasks, and the scaler's task skips a tick while it is still busy with the
    /// one before; this order is one of the interleavings those tasks allow, and the fetch
    /// and the scaler are never skipped. A second call waits for the first to finish.
    pub async fn tick(&self) {
        let mut checks = self.checks.lock().await;
        if let Some(Checks { checker, schedule }) = &mut *checks {
            checker.step(schedule).await;
        }
        self.shared.tick().await;
        self.cached.refresh().await;
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

    /// How many times the agent side could not read or write one of its files since the
    /// server started; always 0 without agent registration. As
    /// [`crate::Running::agent_errors`].
    pub fn agent_errors(&self) -> u64 {
        self.shared.agent_errors()
    }

    /// Drops everything the server holds, with no last tick: what the state files keep is
    /// what the last tick or request wrote, as after a process that ends between two ticks.
    /// Once the routers' clones are dropped too, a second server may open the same files
    /// (`--boot resume`).
    pub fn shutdown(self) {
        drop(self);
    }
}
