//! The job and allocation tables as the server holds them: one [`JobStore`] over the state
//! store chosen at start.
//!
//! The agent side ([`crate::agents::Agents::jobs`]) holds it when the job service is on, and
//! then each heartbeat of an agent that speaks the per-job messages is served from it
//! ([`crate::per_job`]). Without it every heartbeat reply is the lease and run reply alone.

use std::sync::Arc;

use js_jobs::{JobStore, RandomIds};
use js_state::Store;

/// The job store the server runs on: the tables over whichever state store the service
/// opened, with allocation ids from the system's random source.
pub(crate) type ServerJobs = JobStore<Arc<dyn Store>, RandomIds>;
