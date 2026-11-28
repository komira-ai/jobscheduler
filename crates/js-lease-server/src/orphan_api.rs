//! The operator's view and end of a host's orphans: the jobs it lists that the store holds no
//! record of ([`crate::orphans`]).
//!
//! ```text
//! GET /v1/hosts/{host}/orphans
//!
//! 200 {"host": "h1", "orphans": [{"job_id": "j1", "listed_for_secs": 42, "end_asked": false}]}
//!
//! POST /v1/hosts/{host}/orphans/{job_id}/end
//!
//! 202 {"host": "h1", "job_id": "j1", "end": "asked"}
//! ```
//!
//! An orphan keeps running: the server holds no record that says to stop it, and a store that
//! lost its tables would otherwise stop the farm's work. It ends at its own wall limit on the
//! agent, after the server's `--max-job-wall` (one orphan of a host at a time), or when an
//! operator who knows the store is right ends it here. The end is marked in the host's row, so
//! every server over the store tells the host, on each heartbeat while the host lists the
//! job, to stop it (`reason: operator`), and the guard of a host whose listing is mostly
//! unknown does not hold it. The 202 means the end is asked, not done: the listing shows the
//! orphan until the host has stopped it. Asking again is harmless.
//!
//! The routes are behind the client token, like the brake reset and the join-token routes:
//! the server has no operator credential of its own here. A server without the job service,
//! a host the store holds no row of and a job that is not one of the host's orphans are
//! `404` (`unknown-host`, `unknown-orphan`); a store that cannot be read or written is
//! `503 state-write-failed`, and an end whose write a heartbeat of the host overtook is
//! `409 row-moved`, asked again by repeating the call.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use js_jobs::{JobStoreError, NodeRow, NodeState};
use js_types::HostId;
use serde::Serialize;
use tracing::{info, warn};

use crate::agents::Agents;
use crate::api::{Shared, authenticate};
use crate::job_store::ServerJobs;
use crate::util::blocking;
use crate::wire::ApiError;

/// The routes, behind the client token.
pub(crate) fn router(shared: Arc<Shared>, agents: Arc<Agents>) -> Router {
    Router::new()
        .route("/v1/hosts/{host}/orphans", get(list))
        .route("/v1/hosts/{host}/orphans/{job_id}/end", post(end))
        .layer(from_fn_with_state(shared, authenticate))
        .with_state(agents)
}

/// One orphan, as the listing shows it.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct Shown {
    job_id: String,
    /// How long the host has listed it, by the store's clock, in whole seconds.
    listed_for_secs: u64,
    /// Whether an operator asked that the host stop it.
    end_asked: bool,
}

/// The 200 body of the listing.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct Listing {
    host: String,
    orphans: Vec<Shown>,
}

/// The 202 body of an end.
#[derive(Serialize)]
struct Asked {
    host: String,
    job_id: String,
    end: &'static str,
}

/// What an end found.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// The store holds no row of the host.
    NoHost,
    /// The host's row holds no such orphan.
    NoOrphan,
    /// The end is marked, now or before.
    Asked,
    /// A newer write of the host's row came first; nothing was written.
    Moved,
}

/// The orphans of `host` as its row holds them, read at the store's time; `None` for a host
/// the store holds no row of.
fn orphans_of(jobs: &ServerJobs, host: &HostId) -> Result<Option<Listing>, JobStoreError> {
    let Some(row) = jobs.read_node(host)? else {
        return Ok(None);
    };
    let now = jobs.now()?;
    let orphans = row.record.state.orphans.iter().map(|(job, orphan)| Shown {
        job_id: job.clone(),
        listed_for_secs: now.since(orphan.first_seen).as_secs(),
        end_asked: orphan.end_asked,
    });
    Ok(Some(Listing {
        host: host.as_str().to_owned(),
        orphans: orphans.collect(),
    }))
}

/// Marks the orphan `job` of `host` as one an operator ended ([`Ended`]).
fn ask_end(jobs: &ServerJobs, host: &HostId, job: &str) -> Result<Ended, JobStoreError> {
    match jobs.read_node(host)? {
        Some(row) => mark(jobs, &row, job),
        None => Ok(Ended::NoHost),
    }
}

/// Marks the orphan `job` in `row`, at the version it was read at: nothing is written when the
/// row holds no such orphan or already marks it.
fn mark(jobs: &ServerJobs, row: &NodeRow, job: &str) -> Result<Ended, JobStoreError> {
    let mut orphans = row.record.state.orphans.clone();
    let Some(orphan) = orphans.get_mut(job) else {
        return Ok(Ended::NoOrphan);
    };
    if orphan.end_asked {
        return Ok(Ended::Asked);
    }
    orphan.end_asked = true;
    let state = NodeState {
        orphans,
        ..row.record.state.clone()
    };
    let written = jobs.settle_node(row, state)?;
    Ok(written.map_or(Ended::Moved, |_| Ended::Asked))
}

/// The jobs of the server, or the `404` of a server without the job service.
fn jobs_of(agents: &Agents) -> Result<Arc<ServerJobs>, ApiError> {
    agents
        .jobs
        .clone()
        .ok_or(ApiError::coded(StatusCode::NOT_FOUND, "unknown-host"))
}

/// The host id `text` names, or the `404` of one that names none.
fn host_of(text: &str) -> Result<HostId, ApiError> {
    HostId::new(text).map_err(|_| ApiError::coded(StatusCode::NOT_FOUND, "unknown-host"))
}

/// The `503` of a store that could not be read or written.
fn unavailable(host: &HostId, error: &JobStoreError) -> ApiError {
    warn!(%host, %error, "the orphans of a host could not be read or written");
    ApiError::coded(StatusCode::SERVICE_UNAVAILABLE, "state-write-failed")
}

async fn list(
    State(agents): State<Arc<Agents>>,
    Path(host): Path<String>,
) -> Result<Response, ApiError> {
    let (jobs, host) = (jobs_of(&agents)?, host_of(&host)?);
    let read = host.clone();
    let found = blocking(move || orphans_of(&jobs, &read)).await;
    match found.map_err(|error| unavailable(&host, &error))? {
        Some(listing) => Ok((StatusCode::OK, Json(listing)).into_response()),
        None => Err(ApiError::coded(StatusCode::NOT_FOUND, "unknown-host")),
    }
}

async fn end(
    State(agents): State<Arc<Agents>>,
    Path((host, job_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let (jobs, host) = (jobs_of(&agents)?, host_of(&host)?);
    let (named, job) = (host.clone(), job_id.clone());
    let ended = blocking(move || ask_end(&jobs, &named, &job)).await;
    match ended.map_err(|error| unavailable(&host, &error))? {
        Ended::NoHost => Err(ApiError::coded(StatusCode::NOT_FOUND, "unknown-host")),
        Ended::NoOrphan => Err(ApiError::coded(StatusCode::NOT_FOUND, "unknown-orphan")),
        Ended::Moved => Err(ApiError::coded(StatusCode::CONFLICT, "row-moved")),
        Ended::Asked => {
            info!(%host, job = %job_id, "an operator ended an orphan of the host");
            let body = Asked {
                host: host.as_str().to_owned(),
                job_id,
                end: "asked",
            };
            Ok((StatusCode::ACCEPTED, Json(body)).into_response())
        }
    }
}

#[cfg(test)]
#[path = "orphan_api_tests.rs"]
mod tests;
