//! The interim run API: a client that holds a lease on a registered host queues container
//! runs on it, lists them and stops them.
//!
//! Every route is behind the client token, like the lease routes, and exists only when
//! agents register hosts. A run is carried to the host by its next heartbeat reply (see
//! [`js_registry::Registry::heartbeat`]); this module is the consumer's side of that.
//! The run service of the design replaces these routes.
//!
//! A run is a container job (`image`) or a native job (`native`: a program, its arguments
//! and a network level, run on the host by an agent that reported the `sandbox-exec` mode),
//! never both. A native job may pin a bundle (`bundle`, `name[:tag]@sha256:<digest>`) its
//! program is taken from; the host must have reported a bundle store.
//!
//! Runs are in memory only. A restart forgets them, and the hosts that still run any are
//! told to cancel them when they say hello again: a run that was running is killed with no
//! result, and its work is lost. A client that asks after such a run (`GET` or `DELETE` of
//! a run id minted before the restart) is told `410 run-forgotten`, so it can tell a run
//! the restart took from one that never existed (`404 unknown-run`).
//!
//! Tenant isolation is lease-id secrecy under one shared client token: whoever holds the
//! token and knows a lease id (they are sequential) can queue, read and stop that lease's
//! runs. The routes check that a run belongs to the lease in the path, and nothing else.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use js_agent_wire::{
    Isolation, JobSpec, Limits, LogTail, Mount, NativeSpec, NetworkLevel, Outcome, RunFile, limits,
    validate_files,
};
use js_lease::{Clock, LeaseInfo, LeaseState};
use js_registry::{RunRefusal, RunView};
use js_types::LeaseId;
use js_usage::ProfileKey;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::agents::Agents;
use crate::api::{BODY_LIMIT, Shared, authenticate};
use crate::wire::{ApiError, parse, parse_id};

/// What the run handlers share.
#[derive(Clone)]
struct RunApi {
    shared: Arc<Shared>,
    agents: Arc<Agents>,
}

/// The run routes, behind the client token and the body cap of every other route.
pub(crate) fn router(shared: Arc<Shared>, agents: Arc<Agents>) -> Router {
    let api = RunApi {
        shared: shared.clone(),
        agents,
    };
    Router::new()
        .route("/v1/leases/{id}/runs", post(start).get(list))
        .route("/v1/leases/{id}/runs/{run_id}", get(show).delete(stop))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(shared, authenticate))
        .with_state(api)
}

/// The body of `POST /v1/leases/{id}/runs`: a job without the fields the server fills
/// (the ids, the heartbeat it was queued at, how long it stays valid).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBody {
    /// The container image. A job has this or `native`, never both; a body with neither
    /// is a 400 and one with both is a 422 (see [`JobSpec::validate`]).
    #[serde(default)]
    image: Option<String>,
    /// A program to run on the host instead of a container, only on a host that reported
    /// the `sandbox-exec` mode.
    #[serde(default)]
    native: Option<NativeBody>,
    /// The bundle the native job pins, `name[:tag]@sha256:<manifest digest>`; only beside
    /// `native`, and only for a host that reported a bundle store.
    #[serde(default)]
    bundle: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    limits: LimitsBody,
    #[serde(default)]
    gpu_indices: Vec<u32>,
    #[serde(default)]
    mounts: Vec<Mount>,
    /// Secret files the job reads at `/run/js-files/<name>` (see [`RunFile`]).
    #[serde(default)]
    files: Vec<RunFile>,
    #[serde(default)]
    isolation: Isolation,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    /// The name of the profile the run's cost curve is learned under, when the client knows
    /// one: its own key for the work the run does, in the grammar of [`ProfileKey`]. Absent
    /// means the run teaches the server nothing.
    ///
    /// Usage tracks are per lease: when runs of one lease name different profiles, the first
    /// accepted result takes the whole lease series under its own name. The server has one
    /// trust domain: any holder of the client token can name any profile, so a wrong or
    /// hostile run can skew that profile's curve.
    #[serde(default)]
    profile: Option<String>,
}

/// The `native` object of a [`RunBody`]: what a client may ask of a native job. The wire's
/// [`NativeSpec`] has more (environment, working directory, inputs, outputs); they are not
/// on this route, so a key it does not name is a 400.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeBody {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    /// How much network the job may use; absent means none.
    #[serde(default)]
    network: NetworkLevel,
}

impl From<NativeBody> for NativeSpec {
    fn from(body: NativeBody) -> Self {
        Self {
            program: body.program,
            args: body.args,
            env: Default::default(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: body.network,
        }
    }
}

/// The profile name of a [`RunBody`], or the 422 `invalid-profile` when it is not a
/// [`ProfileKey`] (1 to 128 bytes of `0-9`, `a-z`, `.`, `_` and `-`, the first a digit or a
/// letter).
fn profile_of(text: &str) -> Result<ProfileKey, ApiError> {
    ProfileKey::parse(text)
        .map_err(|_| ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, "invalid-profile"))
}

/// The limits of a [`RunBody`]. The wall time is bounded by what the lease has left
/// whatever is asked, and is that bound when left out.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsBody {
    cpu_millis: u64,
    memory_bytes: u64,
    #[serde(default)]
    disk_bytes: Option<u64>,
    #[serde(default)]
    pids: Option<u64>,
    #[serde(default)]
    wall_ms: Option<u64>,
}

/// The 202 body of a queued or stopped run.
#[derive(Debug, Serialize)]
struct Accepted {
    run_id: String,
    state: &'static str,
}

/// One run of the list.
#[derive(Debug, Serialize)]
struct RunEntry {
    run_id: String,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// The labels the client sent when it queued the run; left out when there are none.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    labels: std::collections::BTreeMap<String, String>,
    /// The end of the job's output; only the entry of one run carries it, the list leaves
    /// it out to stay small.
    #[serde(skip_serializing_if = "Option::is_none")]
    log_tail: Option<LogTail>,
}

impl RunEntry {
    /// The entry of one run, with the tail of its output.
    fn with_log(run: RunView) -> Self {
        let log_tail = run
            .result
            .as_ref()
            .and_then(|result| result.log_tail.clone());
        Self {
            log_tail,
            ..Self::from(run)
        }
    }
}

impl From<RunView> for RunEntry {
    /// The entry of the list: no tail.
    fn from(run: RunView) -> Self {
        let result = run.result;
        Self {
            log_tail: None,
            run_id: run.run_id,
            state: run.state.as_str(),
            outcome: result.as_ref().map(|result| result.outcome),
            exit_code: result.as_ref().and_then(|result| result.exit_code),
            detail: result.and_then(|result| result.detail),
            labels: run.labels,
        }
    }
}

/// The body of `GET /v1/leases/{id}/runs`.
#[derive(Debug, Serialize)]
struct RunList {
    runs: Vec<RunEntry>,
}

impl From<RunRefusal> for ApiError {
    /// 404 for a run the lease does not have, 422 for a job that breaks a rule or does
    /// not fit the shape alone, 409 for everything about the state of the lease, the
    /// host and the budget.
    fn from(refusal: RunRefusal) -> Self {
        let status = match refusal {
            RunRefusal::UnknownRun => StatusCode::NOT_FOUND,
            RunRefusal::Invalid(_)
            | RunRefusal::LimitsExceedShape
            | RunRefusal::HostJobsNotAllowed => StatusCode::UNPROCESSABLE_ENTITY,
            RunRefusal::HostNotRegistered
            | RunRefusal::HostLost
            | RunRefusal::NotHeld
            | RunRefusal::HostTainted
            | RunRefusal::NotReady
            | RunRefusal::NoCapacity
            | RunRefusal::FilesUnsupported
            | RunRefusal::DiskUnsupported
            | RunRefusal::NativeUnsupported
            | RunRefusal::BundleUnsupported
            | RunRefusal::RunExists => StatusCode::CONFLICT,
        };
        ApiError::coded(status, refusal.code())
    }
}

/// The 410 for a run id minted before the current boot: the server that held the run has
/// restarted and forgot it, and the work it was doing is lost.
fn remembered(run_id: &str, term: u64) -> Result<(), ApiError> {
    match Agents::is_forgotten(run_id, term) {
        true => Err(ApiError::coded(StatusCode::GONE, "run-forgotten")),
        false => Ok(()),
    }
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, Json(body)).into_response()
}

/// The lease `id` as the table holds it, or the 404.
fn find(leases: Vec<LeaseInfo>, id: &LeaseId) -> Result<LeaseInfo, ApiError> {
    let mut leases = leases.into_iter();
    leases
        .find(|lease| lease.id == *id)
        .ok_or_else(ApiError::unknown_lease)
}

/// Queues a run on the host the lease holds.
///
/// The lease must be held, with time left. The run is bounded by that time: its wall limit
/// is the one asked for, cut to what the lease has left (that, when none is asked), and the
/// agent may start it for no longer than the lease has left either. The registry keeps the
/// instant the lease ends and cuts both again from the time left at each send, so a start
/// delivered late carries less, and none is sent once that time is up. A later renewal does
/// not stretch a run already queued. The table's lock is held across the registry's call,
/// so a release cannot slip in between the check and the queueing.
async fn start(
    State(api): State<RunApi>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = parse_id(&id).ok_or_else(ApiError::unknown_lease)?;
    let body = parse::<RunBody>(&body)?;
    // A job names what runs, an image or a native body: a body with neither is as
    // unreadable as one that lacks a required field.
    if body.image.is_none() & body.native.is_none() {
        let why = "the body names neither an image nor a native job";
        return Err(ApiError::bad_request(why));
    }
    // A bad file is the body's fault, whatever the lease or the host: answered before any
    // state is read. The error names the rule and never repeats a name or contents.
    validate_files(&body.files)
        .map_err(|err| ApiError::coded(StatusCode::BAD_REQUEST, err.code()))?;
    let profile = body.profile.as_deref().map(profile_of).transpose()?;
    let table = api.shared.table().lock().await;
    let lease = find(table.list(), &id)?;
    let ends = lease.expires_at.millis().min(lease.ends_at.millis());
    let left = ends.saturating_sub(api.agents.clock.now().millis());
    let host = match (lease.state, lease.host) {
        (LeaseState::Held, Some(host)) if left > 0 => host,
        _ => return Err(RunRefusal::NotHeld.into()),
    };
    let run_id = api.agents.mint_run_id(table.term());
    let wall_cap = left.min(limits::MAX_WALL_MS);
    let asked = body.limits.wall_ms;
    let spec = JobSpec {
        run_id: run_id.clone(),
        lease_id: id.to_string(),
        image: body.image.unwrap_or_default(),
        args: body.args,
        limits: Limits {
            cpu_millis: body.limits.cpu_millis,
            memory_bytes: body.limits.memory_bytes,
            disk_bytes: body.limits.disk_bytes,
            pids: body.limits.pids,
            wall_ms: asked.map_or(wall_cap, |asked| asked.min(wall_cap)),
        },
        gpu_indices: body.gpu_indices,
        mounts: body.mounts,
        files: body.files,
        isolation: body.isolation,
        labels: body.labels,
        heartbeat_seq: 0,
        valid_for_ms: left.min(limits::MAX_VALID_FOR_MS),
        bundle: body.bundle,
        native: body.native.map(NativeSpec::from),
    };
    let queued = api
        .agents
        .registry
        .queue_keyed_run(&host, &id, spec, profile)?;
    info!(lease = %id, host = %host, run = %run_id, "run queued");
    let accepted = Accepted {
        run_id,
        state: queued.state.as_str(),
    };
    Ok(json(StatusCode::ACCEPTED, &accepted))
}

/// The runs of the lease, oldest first, with the result of each that has one.
async fn list(State(api): State<RunApi>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_id(&id).ok_or_else(ApiError::unknown_lease)?;
    let table = api.shared.table().lock().await;
    find(table.list(), &id)?;
    let runs = api.agents.registry.runs_of(&id);
    let runs = runs.into_iter().map(RunEntry::from).collect();
    Ok(json(StatusCode::OK, &RunList { runs }))
}

/// One run of the lease, as the list shows it, and with the tail of its output (`log_tail`
/// `{stdout, stderr, truncated}`, when the host read one; it is the job's own text, so what
/// the job printed is shown to whoever can read the run). A run id of an earlier boot is 410
/// `run-forgotten` (the server restarted and the run's work is lost); an id the lease
/// does not have is 404 `unknown-run`.
async fn show(
    State(api): State<RunApi>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id).ok_or_else(ApiError::unknown_lease)?;
    let table = api.shared.table().lock().await;
    find(table.list(), &id)?;
    remembered(&run_id, table.term())?;
    let runs = api.agents.registry.runs_of(&id);
    let mut runs = runs.into_iter();
    let run = runs
        .find(|run| run.run_id == run_id)
        .ok_or(RunRefusal::UnknownRun)?;
    Ok(json(StatusCode::OK, &RunEntry::with_log(run)))
}

/// Asks for a run to stop: a retained intent, carried on every heartbeat reply until the
/// host no longer lists the run. The answer is 202 whatever the run's state; a run that
/// has ended stays as it is. A run id of an earlier boot is 410 `run-forgotten`.
async fn stop(
    State(api): State<RunApi>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id).ok_or_else(ApiError::unknown_lease)?;
    let table = api.shared.table().lock().await;
    find(table.list(), &id)?;
    remembered(&run_id, table.term())?;
    let run = api.agents.registry.stop_run(&id, &run_id)?;
    info!(lease = %id, run = %run.run_id, "run stop asked for");
    let accepted = Accepted {
        run_id: run.run_id,
        state: run.state.as_str(),
    };
    Ok(json(StatusCode::ACCEPTED, &accepted))
}
