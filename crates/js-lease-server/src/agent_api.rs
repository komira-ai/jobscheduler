//! The routes agents call, behind the join tokens: the hello and the heartbeat, and the
//! routes a host calls about itself ([`own_host`]).
//!
//! Every answer is a status and `{"error": "<code>"}` with a code from
//! [`js_agent_wire::refusal`], or the message the route returns. Nothing the peer sent
//! reaches an answer: a body that does not decode is answered with the route's
//! `invalid-*` code, never with the decoder's message, which repeats the text it choked
//! on.
//!
//! # Jobs keyed by job id
//!
//! When the server holds the job tables ([`Agents::jobs`]) and the host reported the hello
//! capability [`js_agent_wire::PER_JOB_CAPABILITY`], the heartbeat reply also carries, after
//! the registry has accepted the beat ([`crate::per_job`]):
//!
//! - `commands`: after the lease and run commands, a `job_stop` for each job of the host
//!   with a cancel intent that has not ended, each listed job the store holds no live
//!   allocation of on this host, and each listed job that ended without the host reporting
//!   it; then a `job_start` for each job placed on the host that it does not list yet, with
//!   the allocation's booked resources, the wall limit, the network level and the declared
//!   ports, keyed by the job id and the allocation id. Each is resent until the listing
//!   shows it taken; all of them together stay within `MAX_COMMANDS`.
//! - `job_acks`: a verdict on each listed job result (`ACCEPTED` when this beat committed
//!   the end, `DUPLICATE` when the job had already ended, `STALE` for a job the store holds
//!   no live allocation of here, or an end its state cannot take).
//! - `retired_acks`: every retired id the heartbeat listed, once its job is cancelled and
//!   its allocation freed (or there was nothing to change).
//!
//! The listing's changes (a job starting, running or ended, an allocation freed once the
//! host no longer lists an ended job) are committed before the reply is built. A commit that
//! fails answers 500 `state-write-failed`, with nothing of the reply, and the agent reports
//! again. A reply to any other agent, or from a server without the job tables, is the lease
//! and run reply alone, as it was before these fields: they are left out when empty.
//!
//! # Live logs
//!
//! With `[logs] sink = "object_store"` ([`crate::logs`]), and only to a host whose last hello
//! reported the capability [`js_agent_wire::LOG_UPLOAD_CAPABILITY`]:
//!
//! - a hello the registry accepted queues the mint of the host's upload credential, after the
//!   binding is written (one credential for every job of the host);
//! - each `job_start` carries `log_sink`: the store's URL, region, bucket and style and the
//!   job's own prefix `prefix/job_id/`, with no secret. It is checked as the agent checks it
//!   ([`js_agent_wire::JobStartSpec::log_sink_checked`]) and left out when it fails, and is
//!   left out for a job id the object layout builds no prefix for. It never waits for a
//!   credential: the agent keeps the output in its capture files until one arrives. It rides
//!   in the start, which is resent until the host lists the job;
//! - the reply carries `log_grant`, the host's credential, when the heartbeat's
//!   `log_grant_state.until_ms` is absent or earlier than the credential's expiry: resent until
//!   a heartbeat reports that expiry, and never sent once it has. A heartbeat only reads what
//!   is held in memory and queues a mint when none is held (or when the agent reports `stale`
//!   while it holds the credential held here); it never waits for a mint and never calls the
//!   minter.
//!
//! A server with `sink = "none"`, and a host that did not report the capability, sends
//! neither field, and the reply is byte for byte what it was before them.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Extension, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use js_agent_wire::{Heartbeat, HeartbeatReply, Hello, ProtocolRange, Welcome, refusal};
use js_registry::{Credential, Refusal};
use js_types::HostId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use crate::agents::Agents;
use crate::api::{BODY_LIMIT, bearer};
use crate::fence::STATE_FENCED;
use crate::job_loss;
use crate::job_store::ServerJobs;
use crate::join_tokens::{Admission, JoinRefusal};
use crate::logs::{self, LogGrants};
use crate::per_job::{self, JobReply};
use crate::util::blocking;

/// The code of the 500 a heartbeat is answered when a job or allocation change it reported
/// could not be committed, and of the 503 a hello or a heartbeat is answered when, with the
/// store, the binding it changes could not be committed.
const STATE_WRITE_FAILED: &str = "state-write-failed";

mod own_host;

/// The agent routes. The caller merges them into the server's router; they have no
/// fallback of their own.
pub(crate) fn router(agents: Arc<Agents>) -> Router {
    let hello = post(hello).route_layer(from_fn_with_state(agents.clone(), limit_hello));
    Router::new()
        .route("/v1/agent/hello", hello)
        .route("/v1/agent/heartbeat", post(heartbeat))
        .route(
            "/v1/hosts/{target}",
            get(own_host::show).post(own_host::act),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(agents.clone(), authenticate))
        .with_state(agents)
}

/// Marks a request that was authenticated with a one-time token or a host credential (see
/// [`crate::join_tokens`]) and not with one of the pool's join tokens: the routes then hold
/// it to the host that secret registered. It holds the digest of the secret the request
/// carried, which is the key of its row; the [`Credential`] the registry sees is the one
/// the host was registered under (the same for a token, the token's for a host credential).
#[derive(Clone)]
struct OneTime(Credential);

/// Refuses a request that does not carry one of the join tokens, a one-time token or a host
/// credential, with a 401 that says nothing else, and passes the credential the registry
/// knows the host by on (and the mark [`OneTime`] when it is a one-time token or a host
/// credential). A retired token is not known, so it gets the same 401, as does the client
/// token of the lease routes.
async fn authenticate(
    State(agents): State<Arc<Agents>>,
    mut request: Request,
    next: Next,
) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    let pooled = given.and_then(|given| agents.tokens.identify(given));
    let single = || given.and_then(|given| agents.joins.identify(given));
    match (pooled, single()) {
        (Some(credential), _) => {
            request.extensions_mut().insert(credential);
            next.run(request).await
        }
        (None, Some(presented)) => {
            let registered = agents.joins.registered_as(&presented);
            request.extensions_mut().insert(registered);
            request.extensions_mut().insert(OneTime(presented));
            next.run(request).await
        }
        (None, None) => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Holds a credential to its hello limit before the body is read: a 429 with the code
/// `rate-limited` and a `Retry-After` in whole seconds.
async fn limit_hello(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    request: Request,
    next: Next,
) -> Response {
    match agents.limiter.admit(&who) {
        Ok(()) => next.run(request).await,
        Err(held) => {
            let seconds = retry_after(held.wait);
            if held.announce {
                warn!(
                    seconds,
                    "agent hello held to the rate limit; further ones are not logged until the next interval"
                );
            }
            let reply = Refused::new(StatusCode::TOO_MANY_REQUESTS, refusal::RATE_LIMITED);
            ([(header::RETRY_AFTER, seconds.to_string())], reply).into_response()
        }
    }
}

/// `wait` in whole seconds, rounded up, and at least one.
fn retry_after(wait: Duration) -> u64 {
    wait.as_millis()
        .div_ceil(1000)
        .max(1)
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn hello(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    one_time: Option<Extension<OneTime>>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Refused> {
    let presented = one_time.map(|Extension(OneTime(presented))| presented);
    let welcome = serve_hello(&agents, who, presented, body, &|| ()).await?;
    // The Welcome can carry the host credential, so no cache may keep it (as for a token
    // response in RFC 6749, section 5.1).
    let headers = [
        (header::CACHE_CONTROL, "no-store"),
        (header::PRAGMA, "no-cache"),
    ];
    Ok((headers, welcome).into_response())
}

/// The hello route. `between` runs after a one-time token was admitted and the binding
/// committed, and before the registry is asked, under the gate of the host writes
/// ([`Agents::host_writes`]): no other hello can get in there through the route. Production
/// passes a function that does nothing; a test passes what it checks there, so nothing
/// waits on timing. `who` is the credential the registry knows the host by; `one_time` is
/// the digest of the one-time token or host credential the request carried, when it
/// carried one.
async fn serve_hello(
    agents: &Arc<Agents>,
    who: Credential,
    one_time: Option<Credential>,
    body: Result<Bytes, BytesRejection>,
    between: &(dyn Fn() + Sync),
) -> Result<Json<Welcome>, Refused> {
    let hello: Hello = decode(&read(body, refusal::INVALID_HELLO)?, refusal::INVALID_HELLO)?;
    hello
        .validate()
        .map_err(|err| Refused::of(&Refusal::Invalid(err)))?;
    // Hellos are taken one at a time, from the use of the token to the registry's answer
    // and the release of a refused one, so no other hello of the host comes in between.
    let gate = agents.host_writes.lock().await;
    // The binding the registry will hold once it admits the hello. With the store, when the
    // store does not hold it yet, it is committed before the registry is asked: with the use
    // of the token in one transaction, or alone for a pool token. A known host's binding is
    // held already, and nothing is written for it.
    let host = HostId::new(&hello.host_id).map_err(Refusal::BadHostId)?;
    let change = agents.hello_change(&host, &hello, &who);
    let writes = change.writes.clone();
    // A one-time token registers the host that names its instance, and only that host
    // thereafter. The token is consumed before the registry is asked, so that two hellos
    // cannot both pass; a hello the registry then refuses (the pool is full, the host id
    // is bound to another credential) gives the use back, and the token stays unused for
    // its instance.
    //
    // A hello that says it keeps a host credential, and registers its host with an unused
    // token, retires the token in the same step and is answered with the credential: the
    // token alone opens nothing from then on. When the registry refuses that hello the
    // credential goes back with the use, since the answer that carries it is not sent.
    let mut fresh_use = None;
    let mut issued = None;
    if let Some(presented) = &one_time {
        let instance = hello.instance.clone();
        let host = hello.host_id.clone();
        let asked = hello.accepts_host_credential;
        let admitted = agents
            .admit_join(presented.clone(), (host, instance), asked, writes)
            .await;
        let admitted = admitted.map_err(|refusal| {
            warn!(code = refusal.code(), host = %hello.host_id, "agent hello refused: join token");
            Refused::join(refusal)
        })?;
        if admitted.admission == Admission::Fresh {
            fresh_use = Some(presented.clone());
        }
        issued = admitted.issued;
    } else if let Err(error) = agents.commit_hosts(writes).await {
        warn!(%error, host = %hello.host_id, "agent hello refused: the binding could not be committed");
        return Err(Refused::new(
            StatusCode::SERVICE_UNAVAILABLE,
            STATE_WRITE_FAILED,
        ));
    }
    agents.applied(&change);
    between();
    let mut welcome = match agents.registry.hello(&hello, &who) {
        Ok(welcome) => welcome,
        Err(refusal) => {
            // The compensation: the use of the token and the binding go back in one
            // transaction. When it fails both stay, as the store holds them.
            let undo = change.undo.clone();
            let given_back = match fresh_use {
                Some(presented) => {
                    let host = hello.host_id.clone();
                    agents.release_join(presented, host, undo).await
                }
                None => agents.commit_hosts(undo).await.is_ok(),
            };
            if given_back {
                agents.undone(&change);
            }
            warn!(code = refusal.code(), "agent hello refused");
            return Err(Refused::of(&refusal));
        }
    };
    drop(gate);
    let credential_issued = issued.is_some();
    welcome.host_credential = issued;
    agents.save_bindings().await;
    // A closure over the option, so the route has no arm that only a server with a sink
    // takes: every test binary runs the same code of it.
    let on_hello = |logs: &Arc<LogGrants>| logs.on_hello(&hello.host_id, &hello.capabilities);
    agents.logs.iter().for_each(on_hello);
    info!(host = %hello.host_id, epoch = welcome.epoch, credential_issued, "agent admitted");
    Ok(Json(welcome))
}

async fn heartbeat(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    one_time: Option<Extension<OneTime>>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<HeartbeatReply>, Refused> {
    // Before anything is read or recorded: a server whose store has a newer writer writes
    // nothing at all.
    refuse_when_fenced(&agents)?;
    let heartbeat: Heartbeat = decode(
        &read(body, refusal::INVALID_HEARTBEAT)?,
        refusal::INVALID_HEARTBEAT,
    )?;
    heartbeat
        .validate()
        .map_err(|err| Refused::of(&Refusal::Invalid(err)))?;
    // The load is telemetry. The usage hub looks at its own copy only after the registry
    // has accepted the beat, so a beat the registry refuses records nothing to it; the
    // registry keeps the latest load of the host for `GET /v1/hosts` (it checks the beat
    // before it looks at the load), and nothing about the load can change the reply.
    let load = heartbeat.load.clone();
    if let Some(Extension(OneTime(presented))) = one_time {
        let served = agents
            .serve_join(presented, heartbeat.host_id.clone())
            .await;
        served.map_err(Refused::join)?;
    }
    // The reply carries the starts, stops and cancels for this host and the verdict on
    // each result, and acknowledges a taint report. The agent stops reporting once it is
    // acknowledged, so the report must be durable first. With the store, a taint the beat
    // brings is committed before the registry records it, and a failed commit answers 503
    // `state-write-failed` with nothing recorded, so the agent reports again; in file mode
    // the file is written after, and when it cannot be the reply does not acknowledge it.
    let taint = agents
        .durable_taint(&heartbeat, &who)
        .await
        .map_err(|error| {
            warn!(%error, host = %heartbeat.host_id, "a heartbeat's taint could not be committed");
            Refused::new(StatusCode::SERVICE_UNAVAILABLE, STATE_WRITE_FAILED)
        })?;
    let reply = agents.registry.heartbeat(&heartbeat, &who);
    drop(taint);
    let mut reply = reply?;
    // A start refused as host-tainted taints the host too, with nothing to acknowledge
    // there: the file is made current and a failure is retried by the next tick.
    let refused = heartbeat
        .results
        .iter()
        .any(|result| result.detail.as_deref() == Some(refusal::HOST_TAINTED));
    if reply.taint_ack {
        reply.taint_ack = agents.save_bindings().await;
    } else if refused {
        agents.save_bindings().await;
    }
    let logs = uploading(&agents, &heartbeat.host_id);
    add_jobs(&agents, &heartbeat, logs.clone(), &mut reply).await?;
    reply.log_grant =
        logs.and_then(|logs| logs.on_beat(&heartbeat.host_id, heartbeat.log_grant_state));
    if let Some(usage) = &agents.usage {
        let usage = Arc::clone(usage);
        let acks = reply.result_acks.clone();
        let registry = agents.registry.clone();
        blocking(move || {
            let holds = |lease: &js_types::LeaseId, host: &js_types::HostId| {
                registry.host_of(lease).as_ref() == Some(host)
            };
            usage.on_beat(&heartbeat, load, &acks, &holds);
        })
        .await;
    }
    Ok(Json(reply))
}

/// The host credentials of live logs, when the server has a sink and `host`'s last hello
/// reported [`LOG_UPLOAD_CAPABILITY`]; `None` otherwise, and the reply then carries no log
/// field.
///
/// [`LOG_UPLOAD_CAPABILITY`]: js_agent_wire::LOG_UPLOAD_CAPABILITY
fn uploading(agents: &Agents, host: &str) -> Option<Arc<LogGrants>> {
    let reported = |_: &&Arc<LogGrants>| reports_upload(agents, host);
    agents.logs.as_ref().filter(reported).cloned()
}

/// Whether `host`'s last hello reported [`LOG_UPLOAD_CAPABILITY`].
///
/// [`LOG_UPLOAD_CAPABILITY`]: js_agent_wire::LOG_UPLOAD_CAPABILITY
fn reports_upload(agents: &Agents, host: &str) -> bool {
    HostId::new(host)
        .ok()
        .and_then(|host| agents.registry.own_host(&host))
        .and_then(|own| own.host.claim)
        .is_some_and(|claim| logs::uploads(claim.capabilities()))
}

/// Adds the per-job half of the heartbeat to `reply` when the server holds the job tables
/// and the host reported [`PER_JOB_CAPABILITY`]; leaves it as it is otherwise. Its own
/// function, so the heartbeat route stays the same for every test binary whatever the
/// server holds. The starts carry a log sink when `logs` is given ([`uploading`]).
///
/// [`PER_JOB_CAPABILITY`]: js_agent_wire::PER_JOB_CAPABILITY
async fn add_jobs(
    agents: &Arc<Agents>,
    heartbeat: &Heartbeat,
    logs: Option<Arc<LogGrants>>,
    reply: &mut HeartbeatReply,
) -> Result<(), Refused> {
    let Some(jobs) = &agents.jobs else {
        return Ok(());
    };
    let room = js_agent_wire::limits::MAX_COMMANDS.saturating_sub(reply.commands.len());
    if let Some(served) = serve_jobs(agents, jobs, heartbeat, logs, room).await? {
        reply.commands.extend(served.commands);
        reply.job_acks = served.job_acks;
        reply.retired_acks = served.retired_acks;
    }
    Ok(())
}

/// The per-job half of the heartbeat of a host that reported [`PER_JOB_CAPABILITY`], with
/// room for `room` commands, its starts carrying the sinks of `logs`; `None` for any other
/// host. The store is read and written on the pool for blocking calls. A failed read or
/// commit is a 500 `state-write-failed`, except a store that says it has a newer writer, which
/// is a 503 `state-fenced` and trips the fence.
///
/// [`PER_JOB_CAPABILITY`]: js_agent_wire::PER_JOB_CAPABILITY
async fn serve_jobs(
    agents: &Arc<Agents>,
    jobs: &Arc<ServerJobs>,
    heartbeat: &Heartbeat,
    logs: Option<Arc<LogGrants>>,
    room: usize,
) -> Result<Option<JobReply>, Refused> {
    let per_job = HostId::new(&heartbeat.host_id)
        .ok()
        .and_then(|host| agents.registry.own_host(&host))
        .and_then(|own| {
            let session = (own.host.epoch, own.session_started.map(|at| at.millis()));
            own.host.claim.map(|claim| (own.host.id, session, claim))
        })
        .filter(|(_, _, claim)| {
            let reported = claim.capabilities().get(js_agent_wire::PER_JOB_CAPABILITY);
            reported.map(String::as_str) == Some("true")
        });
    let Some((host, session, claim)) = per_job else {
        return Ok(None);
    };
    let first = agents.first_listing(&host, session);
    let ceiling =
        js_agent_wire::NetworkCeiling::from_capabilities(claim.capabilities()).unwrap_or_default();
    let jobs = Arc::clone(jobs);
    let listed = heartbeat.jobs.clone();
    let retired = heartbeat.retired_jobs.clone();
    let placing = Arc::clone(agents);
    let served = blocking(move || {
        let metrics = placing.meter().sink();
        let context = per_job::BeatContext {
            host: &host,
            ceiling: &ceiling,
            logs: logs.as_deref(),
            metrics,
        };
        // The first listing after a hello loses the jobs that ran and are not listed: their
        // container went with the agent (design section 6.7, step 6). The listing's commit
        // below then frees their allocations, since the host no longer lists them.
        let held: BTreeSet<&str> = listed
            .iter()
            .map(|job| job.job_id.as_str())
            .chain(retired.iter().map(String::as_str))
            .collect();
        let lost = match first {
            true => job_loss::lose_unlisted(&jobs, &host, &held, metrics).map(drop),
            false => Ok(()),
        };
        let served = lost.and_then(|()| per_job::serve(&jobs, &context, &listed, &retired, room));
        // What the listing ended or freed, and the load the beat brought, can make room
        // for a queued job: a pass places it, and its start rides on the host's next reply.
        placing.place_jobs();
        (host, served)
    })
    .await;
    match served {
        (host, Ok(served)) => {
            agents.note_listing(&host, served.clean);
            agents.listed(&host, session);
            Ok(Some(served))
        }
        (host, Err(error)) => {
            agents.meter().sink().store_write_failed();
            warn!(%host, %error, "a heartbeat's job listing could not be committed");
            Err(match agents.fence.note(&error) {
                true => fenced(),
                false => Refused::new(StatusCode::INTERNAL_SERVER_ERROR, STATE_WRITE_FAILED),
            })
        }
    }
}

/// The answer to a heartbeat once the state store has a newer writer: 503 `state-fenced`.
/// The server writes nothing more and is about to exit, so the agent retries against whatever
/// replaces it.
fn fenced() -> Refused {
    Refused::new(StatusCode::SERVICE_UNAVAILABLE, STATE_FENCED)
}

/// [`fenced`] when the fence has tripped, else nothing.
fn refuse_when_fenced(agents: &Agents) -> Result<(), Refused> {
    match agents.fence.is_tripped() {
        true => Err(fenced()),
        false => Ok(()),
    }
}

/// The body of a request, or the refusal for a body that is over the cap (413) or could
/// not be read (400, with `malformed` as its code).
fn read(body: Result<Bytes, BytesRejection>, malformed: &'static str) -> Result<Bytes, Refused> {
    body.map_err(|rejection| match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => {
            Refused::new(StatusCode::PAYLOAD_TOO_LARGE, refusal::BODY_TOO_LARGE)
        }
        _ => Refused::new(StatusCode::BAD_REQUEST, malformed),
    })
}

/// A message from JSON. A body that is not the message is a 400 with `malformed` and
/// nothing of the decoder's complaint.
fn decode<T: DeserializeOwned>(bytes: &[u8], malformed: &'static str) -> Result<T, Refused> {
    serde_json::from_slice(bytes).map_err(|_| Refused::new(StatusCode::BAD_REQUEST, malformed))
}

/// An error answer: a status and `{"error": "<code>"}`, with the majors the server
/// speaks when the code is `unsupported-protocol`.
#[derive(Debug, Serialize)]
pub(crate) struct Refused {
    #[serde(skip)]
    status: StatusCode,
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    supported: Option<ProtocolRange>,
    /// The capability at fault, for the two refusals of the host contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'static str>,
}

impl Refused {
    fn new(status: StatusCode, error: &'static str) -> Self {
        Self {
            status,
            error,
            supported: None,
            field: None,
        }
    }

    /// The answer to a refused one-time token: 503 when the use could not be written down or
    /// the credential could not be made, 403 otherwise. The code names the fault and
    /// nothing of the token.
    fn join(refusal: JoinRefusal) -> Self {
        let status = match refusal {
            JoinRefusal::Unsaved | JoinRefusal::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::FORBIDDEN,
        };
        Self::new(status, refusal.code())
    }

    /// The answer to a registry refusal: 400 for a message that breaks the wire's rules,
    /// 409 for a session the agent should say hello again for (or wait out), 403 for
    /// what the credential or the pool does not allow, 503 when no session secret could
    /// be made.
    fn of(refusal: &Refusal) -> Self {
        let status = match refusal {
            Refusal::Invalid(_) | Refusal::BadHostId(_) | Refusal::BadHeartbeatHostId(_) => {
                StatusCode::BAD_REQUEST
            }
            Refusal::HostLive | Refusal::StaleEpoch | Refusal::UnknownHost => StatusCode::CONFLICT,
            Refusal::UnknownPool
            | Refusal::HostBound
            | Refusal::HostListed
            | Refusal::ShapeNotAccepted
            | Refusal::HostContract { .. }
            | Refusal::ExecutionMismatch { .. }
            | Refusal::PoolFull
            | Refusal::SessionMismatch => StatusCode::FORBIDDEN,
            Refusal::SessionUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        let code = refusal.code();
        let unsupported = code == js_agent_wire::refusal::UNSUPPORTED_PROTOCOL;
        Self {
            supported: unsupported.then_some(ProtocolRange::THIS),
            field: refusal.field(),
            ..Self::new(status, code)
        }
    }
}

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Self::of(&refusal)
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        (self.status, Json(&self)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use js_agent_wire::Phase;
    use js_types::HostId;
    use serde_json::json;

    use super::*;

    /// Every refusal of the registry has the status and the wire code the API documents:
    /// 400 for a message that breaks the wire's rules, 409 for the refusals that tell an
    /// agent to say hello again or wait, 403 for what the credential or the pool does not
    /// allow, 503 when no session secret could be made; and only `unsupported-protocol`
    /// carries the majors served. It catches a new refusal that falls into the wrong class
    /// of status, and the 503, which no transport test can provoke because the registry
    /// draws its secrets from the operating system.
    #[test]
    fn every_refusal_has_its_status_and_code() {
        let unnamed = Heartbeat {
            host_id: "UPPER".into(),
            epoch: 0,
            session: String::new(),
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
            log_grant_state: None,
        };
        let hello: Hello = serde_json::from_value(json!({
            "protocol": { "min": 2, "max": 3 },
            "host_id": "h",
            "pool": "p",
            "agent_version": "1",
            "shape": { "cpu_millis": 0, "memory_bytes": 0, "gpus": 0 },
            "phase": "serving",
        }))
        .unwrap();
        let bad_id = HostId::new("").unwrap_err();
        let cases = [
            (
                Refusal::Invalid(unnamed.validate().unwrap_err()),
                400,
                "invalid-heartbeat",
            ),
            (
                Refusal::Invalid(hello.validate().unwrap_err()),
                400,
                "unsupported-protocol",
            ),
            (Refusal::BadHostId(bad_id.clone()), 400, "invalid-hello"),
            (
                Refusal::BadHeartbeatHostId(bad_id),
                400,
                "invalid-heartbeat",
            ),
            (Refusal::HostLive, 409, "host-live"),
            (Refusal::StaleEpoch, 409, "stale-epoch"),
            (Refusal::UnknownHost, 409, "unknown-host"),
            (Refusal::UnknownPool, 403, "unknown-pool"),
            (Refusal::HostBound, 403, "host-bound"),
            (Refusal::HostListed, 403, "host-listed"),
            (Refusal::ShapeNotAccepted, 403, "shape-not-accepted"),
            (
                Refusal::HostContract { field: "runtime" },
                403,
                "host-contract",
            ),
            (
                Refusal::ExecutionMismatch { field: "exec" },
                403,
                "execution-mismatch",
            ),
            (Refusal::PoolFull, 403, "pool-full"),
            (Refusal::SessionMismatch, 403, "session-mismatch"),
            (Refusal::SessionUnavailable, 503, "session-unavailable"),
        ];
        for (refusal, status, code) in cases {
            let answer = Refused::of(&refusal);
            assert_eq!(answer.status.as_u16(), status, "{code}");
            assert_eq!(answer.error, code);
            let says_majors = code == "unsupported-protocol";
            assert_eq!(answer.supported.is_some(), says_majors, "{code}");
            assert_eq!(answer.field, refusal.field(), "{code}");
        }
    }

    /// Every refusal of a one-time token has its status and code: 503 for the use that
    /// could not be written down (the agent may try again), 403 for the rest.
    #[test]
    fn every_join_refusal_has_its_status_and_code() {
        let cases = [
            (JoinRefusal::Expired, 403, "join-token-expired"),
            (JoinRefusal::Used, 403, "join-token-used"),
            (JoinRefusal::WrongInstance, 403, "join-token-instance"),
            (JoinRefusal::NotAdmitted, 403, "join-token-not-admitted"),
            (JoinRefusal::Unsaved, 503, "join-token-unsaved"),
            (JoinRefusal::Unavailable, 503, "join-token-unavailable"),
            (
                JoinRefusal::CredentialRequired,
                403,
                "join-token-credential-required",
            ),
        ];
        for (refusal, status, code) in cases {
            let answer = Refused::join(refusal);
            assert_eq!(answer.status.as_u16(), status, "{code}");
            assert_eq!(answer.error, code);
            assert!(answer.supported.is_none());
        }
    }

    /// A wait is told in whole seconds rounded up, never zero, whatever the fraction: a
    /// client told zero would retry at once, and one told too little retries into the
    /// limit again.
    #[test]
    fn a_wait_rounds_up_to_whole_seconds_and_is_never_zero() {
        let seconds = |millis| retry_after(Duration::from_millis(millis));
        assert_eq!(seconds(0), 1);
        assert_eq!(seconds(1), 1);
        assert_eq!(seconds(1000), 1);
        assert_eq!(seconds(1001), 2);
        assert_eq!(seconds(7000), 7);
        assert_eq!(retry_after(Duration::MAX), u64::MAX);
    }
}

#[cfg(test)]
#[path = "agent_api_race_tests.rs"]
mod race_tests;

#[cfg(test)]
#[path = "agent_api_store_tests.rs"]
mod store_tests;

#[cfg(test)]
#[path = "agent_api_job_rig.rs"]
pub(crate) mod job_rig;

#[cfg(test)]
#[path = "agent_api_job_tests.rs"]
mod job_tests;

#[cfg(test)]
#[path = "agent_api_job_cases.rs"]
mod job_cases;

#[cfg(test)]
#[path = "agent_api_log_tests.rs"]
mod log_tests;

#[cfg(test)]
#[path = "agent_api_log_life_tests.rs"]
mod log_life_tests;
