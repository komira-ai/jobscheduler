//! The routes a host calls about itself, with the credential it was admitted with:
//!
//! - `POST /v1/hosts/{id}:cordon`: cordon the host for `lease_secs` seconds (an optional
//!   JSON body `{"lease_secs": n, "holder": "<id>", "take_over": true}`; every field optional,
//!   [`cordon::DEFAULT_CORDON`] when `lease_secs` is absent). The server cuts the length to
//!   [`cordon::MAX_CORDON`]. Who asks decides what the call does ([`js_types::cordon`]): with
//!   no cordon in force it places one, held by the `holder` named (or by none); a call that
//!   names no holder extends any cordon in force and never shortens it; a call of the cordon's
//!   own holder renews it the same way; a call of another holder leaves the cordon in force as
//!   it is and answers `held_by_other: true`; `take_over` (with a holder only) makes the
//!   cordon in force the caller's.
//! - `POST /v1/hosts/{id}:uncordon`: lift the cordon; 200 whether or not there was one. With
//!   a body `{"holder": "<id>"}` only that holder's cordon is lifted, and another's is left
//!   with `held_by_other: true`.
//! - `GET /v1/hosts/{id}`: the host's state, as the agent's update hook waits on it.
//!
//! A holder is an id (1 to 63 characters of `[a-z0-9._-]`); a holder that is not one, or a
//! take-over that names none, is 400 `invalid-cordon`. The reading of a cordoned host names
//! the cordon's holder, when it has one.
//!
//! The host id in the path must be the host of the credential: a host credential (or the
//! one-time token that registered the host), whose host the server wrote down at the
//! hello. The shared pool token names no host and is refused. Every other host id is the
//! same 403 as a credential that never was the host's, so the answer tells nothing of
//! which hosts exist. A host id that passes and that the registry does not hold (it has
//! not said hello since the server started and holds nothing a restart puts back) is
//! 409 `unknown-host`, as for a heartbeat: the agent says hello again.
//!
//! A request whose connection carries a host certificate (see [`super::host_cert`]) is
//! served with no other credential, and the path's host must be the certificate's, or it is
//! 403 `certificate-host-mismatch`.
//!
//! There is no server-side total cap: a caller that dies leaves its cordon to lapse within
//! its last `lease_secs`, at most [`cordon::MAX_CORDON`], and a caller of another holder does
//! not extend it meanwhile.
//!
//! A cordon, and its lift, is made durable before it is acknowledged, and the answer is
//! 503 `cordon-unsaved` when that fails; the same call repeated is the retry. With the job
//! service on, the job store's row is the cordon's record and is written first (see
//! `crate::agents` for the cordon's module): every server over the store keeps the host out
//! of placement from then on, even when the binding's write after it fails. Without it, the
//! binding is written to the agent state file after the registry took it, so the cordon holds
//! on this server all the same, and until a save succeeds a server restart loses that cordon,
//! or brings back one that an unsaved uncordon lifted (for at most the length it was asked for:
//! [`cordon::MAX_CORDON`] on this route, seven days for an operator's). Every call, the read too, checks the credential with
//! `JoinTokens::serves`, which renews the host credential: in file mode it may write the join
//! token file, and a failed write is logged and does not fail the call; with the store the
//! renewal is left for the tick, and the call makes no store call.

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
use js_types::cordon::Asker;
use js_types::{CordonHolder, HostId};
use serde::{Deserialize, Serialize};

use super::host_cert::{self, CertifiedHost};
use super::{OneTime, Refused, decode, read};
use crate::agents::{Agents, CordonFault};
use crate::cordon::{self, CORDON_UNSAVED, INVALID_CORDON, UNKNOWN_HOST};
use crate::hosts::{mark_text, state_text};
use crate::join_tokens::JoinRefusal;

/// The pool token, or no one-time token or credential, names no host.
const HOST_CREDENTIAL_REQUIRED: &str = "host-credential-required";
const NOT_FOUND: &str = "not-found";

/// The body of a cordon request.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CordonRequest {
    /// How long the cordon lasts from now, in seconds; at least one.
    lease_secs: Option<u64>,
    /// Who asks: the cordon's holder, when the caller names one.
    holder: Option<String>,
    /// Whether the caller takes the cordon in force over, whoever holds it (with a holder).
    take_over: Option<bool>,
}

/// The body of an uncordon request.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UncordonRequest {
    /// The holder whose cordon to lift; none lifts any cordon.
    holder: Option<String>,
}

/// When a cordon lapses, and who holds it.
#[derive(Debug, Serialize)]
struct CordonView {
    expires_at_ms: u64,
    expires_in_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<String>,
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
    /// The number of leases on the host: always 0, since no lease is booked on a host any
    /// more. It stays in the reading because an agent of the previous release requires it.
    leases: usize,
    /// Whether the host holds no lease but has runs to finish. It stays in the reading for
    /// the same reason as `leases`.
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
    /// For a cordon or an uncordon: whether the cordon in force is another holder's, left as
    /// it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    held_by_other: Option<bool>,
}

/// What a cordon or an uncordon did, for its reading: `already_cordoned` for a cordon,
/// `lifted` for an uncordon, and `held_by_other` for both.
#[derive(Debug, Default, Clone, Copy)]
struct Did {
    already_cordoned: Option<bool>,
    lifted: Option<bool>,
    held_by_other: Option<bool>,
}

impl Reading {
    fn of(own: OwnHost, did: Did) -> Self {
        let now = own.now.millis();
        let host = own.host;
        Self {
            id: host.id.to_string(),
            state: state_text(host.state),
            epoch: host.epoch,
            session_started_ms: own.session_started.map(|at| at.millis()),
            leases: host.leases.len(),
            releasing: host.releasing,
            mark: host.mark.map(mark_text),
            cordon: own.cordon_until.map(|until| CordonView {
                expires_at_ms: until.millis(),
                expires_in_ms: until.millis().saturating_sub(now),
                holder: own.cordon_holder.map(|holder| holder.to_string()),
            }),
            taint: host.taint,
            agent_version: host.agent_version,
            runtime: own.runtime,
            already_cordoned: did.already_cordoned,
            lifted: did.lifted,
            held_by_other: did.held_by_other,
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

/// The host of the request, if `host` is the host of the credential the request carries (a
/// certificate's host is checked as [`host_cert::check`] does).
fn authorize(
    agents: &Agents,
    (one_time, certified): (Option<Extension<OneTime>>, Option<Extension<CertifiedHost>>),
    host: &str,
) -> Result<HostId, Refused> {
    let id = match (certified, one_time) {
        (Some(Extension(certified)), _) => {
            host_cert::check(Some(&certified), host)?;
            certified.0
        }
        (None, Some(Extension(OneTime(presented)))) => {
            // An id that cannot be a host's is not the credential's host: the answer is the
            // one for any other host.
            let id = HostId::new(host).map_err(|_| Refused::join(JoinRefusal::NotAdmitted))?;
            agents
                .joins
                .serves(&presented, host)
                .map_err(Refused::join)?;
            id
        }
        (None, None) => {
            return Err(Refused::new(
                StatusCode::FORBIDDEN,
                HOST_CREDENTIAL_REQUIRED,
            ));
        }
    };
    Ok(id)
}

/// The answer to a cordon or uncordon that was not done (409 `unknown-host`) or not made
/// durable (503 `cordon-unsaved`).
fn refused(fault: CordonFault) -> Refused {
    match fault {
        CordonFault::NotHeld => unknown(NotHeld),
        CordonFault::Unsaved => Refused::new(StatusCode::SERVICE_UNAVAILABLE, CORDON_UNSAVED),
    }
}

/// The refusal of a cordon request that does not read.
fn invalid() -> Refused {
    Refused::new(StatusCode::BAD_REQUEST, INVALID_CORDON)
}

/// The holder `name` names, if it names one.
fn holder_of(name: Option<String>) -> Result<Option<CordonHolder>, Refused> {
    let holder = name.map(|name| CordonHolder::new(&name));
    holder.transpose().map_err(|_| invalid())
}

/// A body as `T`; an empty body is `T`'s default.
fn body_of<T: Default + serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Refused> {
    match body.is_empty() {
        true => Ok(T::default()),
        false => decode(body, INVALID_CORDON),
    }
}

/// The length of the cordon a body asks for, as [`cordon::length`] gives it, and who asks.
fn cordon_of(body: &[u8]) -> Result<(Duration, Asker), Refused> {
    let request: CordonRequest = body_of(body)?;
    let lease = cordon::length(request.lease_secs).ok_or_else(invalid)?;
    let holder = holder_of(request.holder)?;
    let by = match (holder, request.take_over.unwrap_or(false)) {
        (None, false) => Asker::Unnamed,
        (None, true) => return Err(invalid()),
        (Some(holder), false) => Asker::Holder(holder),
        (Some(holder), true) => Asker::TakeOver(holder),
    };
    Ok((lease, by))
}

/// `GET /v1/hosts/{id}`.
pub(super) async fn show(
    State(agents): State<Arc<Agents>>,
    who: (Option<Extension<OneTime>>, Option<Extension<CertifiedHost>>),
    Path(target): Path<String>,
) -> Result<Response, Refused> {
    let (host, verb) = split(&target);
    let host = authorize(&agents, who, host)?;
    match verb {
        Some(_) => Err(not_found()),
        None => {
            let own = agents.registry.own_host(&host);
            let own = own.ok_or(NotHeld).map_err(unknown)?;
            Ok(Json(Reading::of(own, Did::default())).into_response())
        }
    }
}

/// `POST /v1/hosts/{id}:cordon` and `POST /v1/hosts/{id}:uncordon`.
pub(super) async fn act(
    State(agents): State<Arc<Agents>>,
    who: (Option<Extension<OneTime>>, Option<Extension<CertifiedHost>>),
    Path(target): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Refused> {
    let (host, verb) = split(&target);
    let host = authorize(&agents, who, host)?;
    let body = read(body, INVALID_CORDON)?;
    match verb {
        Some("cordon") => {
            let (lease, by) = cordon_of(&body)?;
            let placed = agents.cordon(&host, lease, by).await.map_err(refused)?;
            let did = Did {
                already_cordoned: Some(!placed.placed),
                held_by_other: Some(placed.held_by_other),
                ..Did::default()
            };
            Ok(Json(Reading::of(placed.host, did)).into_response())
        }
        Some("uncordon") => {
            let request: UncordonRequest = body_of(&body)?;
            let holder = holder_of(request.holder)?;
            let lifted = agents.uncordon(&host, holder).await.map_err(refused)?;
            let did = Did {
                lifted: Some(lifted.lifted),
                held_by_other: Some(lifted.held_by_other),
                ..Did::default()
            };
            Ok(Json(Reading::of(lifted.host, did)).into_response())
        }
        _ => Err(not_found()),
    }
}
