//! The routes a host calls about itself, with the credential it was admitted with:
//!
//! - `POST /v1/hosts/{id}:cordon`: cordon the host for `lease_secs` seconds (an optional
//!   JSON body `{"lease_secs": n}`; [`DEFAULT_CORDON`] when absent). A repeat renews and
//!   never shortens. The server cuts the length to [`MAX_CORDON`].
//! - `POST /v1/hosts/{id}:uncordon`: lift the cordon; 200 whether or not there was one.
//! - `GET /v1/hosts/{id}`: the host's state, as the agent's update hook waits on it.
//!
//! The host id in the path must be the host of the credential: a host credential (or the
//! one-time token that registered the host), whose host the server wrote down at the
//! hello. The shared pool token names no host and is refused. Every other host id is the
//! same 403 as a credential that never was the host's, so the answer tells nothing of
//! which hosts exist. A host id that passes and that the registry does not hold (it has
//! not said hello since the server started and holds nothing a restart puts back) is
//! 409 `unknown-host`, as for a heartbeat: the agent says hello again.
//!
//! The server keeps no record of who placed a cordon, and an uncordon lifts any cordon.
//! A caller therefore uncordons only a cordon it placed, and takes `already_cordoned:
//! false` in the reply of its cordon as the proof that it placed it. There is no
//! server-side total cap: a caller that dies leaves its cordon to lapse within its last
//! `lease_secs`, at most [`MAX_CORDON`].
//!
//! A cordon is written to the agent state file before it is acknowledged: when that fails
//! the answer is 503 `cordon-unsaved`, the cordon holds on this server all the same, and
//! the same call repeated is the retry. Until a save succeeds a server restart loses that
//! cordon, or brings back one that an unsaved uncordon lifted (for at most the
//! [`MAX_CORDON`] it was asked for). Every call, the read too, checks the credential
//! with `JoinTokens::serves`, which renews the host credential and may write the join
//! token file; a failed write is logged and does not fail the call.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_agent_wire::Taint;
use js_registry::{NotHeld, OwnHost};
use js_types::HostId;
use serde::{Deserialize, Serialize};

use super::{OneTime, Refused, decode, read};
use crate::agents::Agents;
use crate::hosts::{mark_text, state_text};
use crate::join_tokens::JoinRefusal;

/// The length of a cordon asked for without one.
pub(crate) const DEFAULT_CORDON: Duration = Duration::from_secs(5 * 60);

/// The longest a cordon is set for by one request; a longer one asked for is cut to it. A
/// requester that needs a cordon for longer renews it.
pub(crate) const MAX_CORDON: Duration = Duration::from_secs(60 * 60);

/// The pool token, or no one-time token or credential, names no host.
const HOST_CREDENTIAL_REQUIRED: &str = "host-credential-required";
const INVALID_CORDON: &str = "invalid-cordon";
const CORDON_UNSAVED: &str = "cordon-unsaved";
const UNKNOWN_HOST: &str = "unknown-host";
const NOT_FOUND: &str = "not-found";

/// The body of a cordon request.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CordonRequest {
    /// How long the cordon lasts from now, in seconds; at least one.
    lease_secs: Option<u64>,
}

/// When a cordon lapses.
#[derive(Debug, Serialize)]
struct CordonView {
    expires_at_ms: u64,
    expires_in_ms: u64,
}

/// What the three routes answer: the host, and what the call did.
#[derive(Debug, Serialize)]
struct Reading {
    id: String,
    /// `awaiting`, `live` or `lost`, as `GET /v1/hosts` says.
    state: &'static str,
    /// The session's epoch: one more at each hello of the host, and 0 for a host that has
    /// not said hello since the server started (it starts again from 1 after a restart).
    epoch: u64,
    /// When the current session began, in milliseconds on the server's clock: the time of
    /// its hello. A reading of a host that went silent keeps the session it had, so a
    /// reader waiting for a new agent looks for a later value here (or a higher epoch),
    /// not for `live`. Absent until the host says hello after a server restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_started_ms: Option<u64>,
    /// The leases the host holds: 0 or 1.
    leases: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease: Option<String>,
    /// The host holds no lease but has runs to finish.
    releasing: bool,
    /// How the host is set aside, as `GET /v1/hosts` lists it: `reserved`, `held`, `draining` or
    /// `releasing`. Absent for a host that is not set aside.
    #[serde(skip_serializing_if = "Option::is_none")]
    mark: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cordon: Option<CordonView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    taint: Option<Taint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_version: Option<String>,
    /// The `runtime` of the host's last hello.
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    /// For a cordon: whether the host was cordoned before this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    already_cordoned: Option<bool>,
    /// For an uncordon: whether a cordon was in force and this call ended it.
    #[serde(skip_serializing_if = "Option::is_none")]
    lifted: Option<bool>,
}

impl Reading {
    fn of(own: OwnHost, already_cordoned: Option<bool>, lifted: Option<bool>) -> Self {
        let now = own.now.millis();
        let host = own.host;
        Self {
            id: host.id.to_string(),
            state: state_text(host.state),
            epoch: host.epoch,
            session_started_ms: own.session_started.map(|at| at.millis()),
            leases: usize::from(host.lease.is_some()),
            lease: host.lease.map(|lease| lease.to_string()),
            releasing: host.releasing,
            mark: host.mark.map(mark_text),
            cordon: own.cordon_until.map(|until| CordonView {
                expires_at_ms: until.millis(),
                expires_in_ms: until.millis().saturating_sub(now),
            }),
            taint: host.taint,
            agent_version: host.agent_version,
            runtime: own.runtime,
            already_cordoned,
            lifted,
        }
    }
}

fn not_found() -> Refused {
    Refused::new(StatusCode::NOT_FOUND, NOT_FOUND)
}

fn unknown(_: NotHeld) -> Refused {
    Refused::new(StatusCode::CONFLICT, UNKNOWN_HOST)
}

/// The host id and the verb of the last path segment: `burst-1:cordon` is `burst-1` and
/// `cordon`. A host id has no colon.
fn split(target: &str) -> (&str, Option<&str>) {
    match target.split_once(':') {
        Some((host, verb)) => (host, Some(verb)),
        None => (target, None),
    }
}

/// The host of the request, if `host` is the host of the credential the request carries.
fn authorize(
    agents: &Agents,
    one_time: Option<Extension<OneTime>>,
    host: &str,
) -> Result<HostId, Refused> {
    let Some(Extension(OneTime(presented))) = one_time else {
        return Err(Refused::new(
            StatusCode::FORBIDDEN,
            HOST_CREDENTIAL_REQUIRED,
        ));
    };
    // An id that cannot be a host's is not the credential's host: the answer is the one
    // for any other host.
    let id = HostId::new(host).map_err(|_| Refused::join(JoinRefusal::NotAdmitted))?;
    agents
        .joins
        .serves(&presented, host)
        .map_err(Refused::join)?;
    Ok(id)
}

/// Makes the file hold what the registry has bound, or the 503 that says it does not.
async fn saved(agents: &Arc<Agents>) -> Result<(), Refused> {
    match agents.save_bindings().await {
        true => Ok(()),
        false => Err(Refused::new(
            StatusCode::SERVICE_UNAVAILABLE,
            CORDON_UNSAVED,
        )),
    }
}

/// The length of the cordon a body asks for, cut to [`MAX_CORDON`].
fn lease_of(body: &[u8]) -> Result<Duration, Refused> {
    let request: CordonRequest = match body.is_empty() {
        true => CordonRequest::default(),
        false => decode(body, INVALID_CORDON)?,
    };
    let secs = request.lease_secs.unwrap_or(DEFAULT_CORDON.as_secs());
    match secs {
        0 => Err(Refused::new(StatusCode::BAD_REQUEST, INVALID_CORDON)),
        secs => Ok(Duration::from_secs(secs).min(MAX_CORDON)),
    }
}

/// `GET /v1/hosts/{id}`.
pub(super) async fn show(
    State(agents): State<Arc<Agents>>,
    one_time: Option<Extension<OneTime>>,
    Path(target): Path<String>,
) -> Result<Response, Refused> {
    let (host, verb) = split(&target);
    let host = authorize(&agents, one_time, host)?;
    match verb {
        Some(_) => Err(not_found()),
        None => {
            let own = agents.registry.own_host(&host);
            let own = own.ok_or(NotHeld).map_err(unknown)?;
            Ok(Json(Reading::of(own, None, None)).into_response())
        }
    }
}

/// `POST /v1/hosts/{id}:cordon` and `POST /v1/hosts/{id}:uncordon`.
pub(super) async fn act(
    State(agents): State<Arc<Agents>>,
    one_time: Option<Extension<OneTime>>,
    Path(target): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Refused> {
    let (host, verb) = split(&target);
    let host = authorize(&agents, one_time, host)?;
    let body = read(body, INVALID_CORDON)?;
    match verb {
        Some("cordon") => {
            let lease = lease_of(&body)?;
            let placed = agents.registry.cordon(&host, lease).map_err(unknown)?;
            saved(&agents).await?;
            let already = Some(!placed.placed);
            Ok(Json(Reading::of(placed.host, already, None)).into_response())
        }
        Some("uncordon") => {
            let lifted = agents.registry.uncordon(&host).map_err(unknown)?;
            saved(&agents).await?;
            Ok(Json(Reading::of(lifted.host, None, Some(lifted.lifted))).into_response())
        }
        _ => Err(not_found()),
    }
}
