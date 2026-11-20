//! The routes agents call, behind the join tokens or the host's certificate
//! ([`host_cert`]): the hello and the heartbeat, and the routes a host calls about itself
//! ([`own_host`]).
//!
//! Every answer is a status and `{"error": "<code>"}` with a code from
//! [`js_agent_wire::refusal`], or the message the route returns. Nothing the peer sent
//! reaches an answer: a body that does not decode is answered with the route's
//! `invalid-*` code, never with the decoder's message, which repeats the text it choked
//! on.
//!
//! # Any server serves any beat
//!
//! When the server holds the job tables, a host's session is its store row `node/<host>`
//! ([`rows`]): the hello writes it, a heartbeat of an epoch this server's registry does not
//! hold takes the session from it, and every heartbeat is applied to it, guarded by its
//! (epoch, sequence), before its listing is committed with every write conditional on the
//! version the beat wrote. So a heartbeat that reaches another server than the hello's is
//! served there, with no second hello; a beat older than one the row applied, or whose listing
//! a newer beat overtook, changes nothing more and is answered from the rows, and is not held.
//! Without the job tables the session is the registry's alone, as before.
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
//!   ports, keyed by the job id and the allocation id, and `isolation: host` for a job that
//!   runs on the machine itself (sent only to a host that reported `host-jobs`). Each is
//!   resent until the listing shows it taken; all of them together stay within
//!   `MAX_COMMANDS`. A job that books the whole host is neither stopped nor started while it
//!   is placed (the host prepares), and is started once it is starting;
//! - `exclusive`: the allocation id of the whole-host job the host is to prepare for or
//!   keep, until its job ends;
//! - `job_acks`: a verdict on each listed job result (`ACCEPTED` when this beat committed
//!   the end, `DUPLICATE` when the job had already ended, `STALE` for a job the store holds
//!   no live allocation of here, or an end its state cannot take).
//! - `retired_acks`: every retired id the heartbeat listed, once its job is cancelled and
//!   its allocation freed (or there was nothing to change).
//!
//! Before the listing, the heartbeat's `phase`, `exclusive_ready` and `exclusive_refused`
//! move the placed whole-host job of the host ([`crate::job_whole`]): a ready report starts
//! it, a refusal fails its prepare, the first listing after a hello (the row's flag) restarts
//! it, and a cancel
//! ends it once the agent has left `preparing`.
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
//!
//! # Held replies
//!
//! To a host that also reported [`js_agent_wire::HELD_BEAT_CAPABILITY`], a reply with nothing
//! to send is held open until a job is placed on the host, or for the heartbeat interval at
//! most five seconds, and says `beat_now` ([`held`], [`crate::held_beat`]). The host's
//! heartbeat is watched from before the store is read, so a placement after the read wakes
//! it; the reply is then built again from the store and carries the start. A heartbeat that
//! lists a job as `starting` is held for [`crate::held_beat::starting_hold`] (200 ms) at most
//! instead, so the start's `running` is not kept a whole hold.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Extension, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use js_agent_wire::{Heartbeat, HeartbeatReply, Hello, HostCredential, Phase, Welcome, refusal};
use js_jobs::{JobStoreError, NodeRow};
use js_registry::{Credential, Refusal};
use js_types::{HostId, Resources, Shape};
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use self::host_cert::{CertifiedHost, admit_hello};
use crate::admit::{
    reported_mount_providers, reports_held_beat, reports_host_jobs, reports_layered_rootfs,
};
use crate::agent_state::Change;
use crate::agents::Agents;
use crate::api::{BODY_LIMIT, bearer};
use crate::host_keys::Writes;
use crate::job_loss;
use crate::job_store::ServerJobs;
use crate::job_whole;
use crate::join_tokens::Admission;
use crate::logs::{self, LogGrants};
use crate::per_job::{self, JobReply};
use crate::util::blocking;

/// The code of the 500 a heartbeat is answered when a job or allocation change it reported
/// could not be committed, and of the 503 a hello or a heartbeat is answered when, with the
/// store, the binding it changes could not be committed.
const STATE_WRITE_FAILED: &str = "state-write-failed";

mod held;
mod host_cert;
mod limit;
mod own_host;
mod refused;
pub(crate) mod rows;
mod unknown;

pub(crate) use self::refused::Refused;
use self::unknown::{identify_single, unknown_bearer};

/// The watch of a heartbeat whose reply may be held, and what building it again needs.
type Watched = (crate::held_beat::Watch, Arc<held::Inputs>);

/// What the authentication left on a hello: the mark of a one-time token or host credential,
/// of a pool's shared token, and of a host certificate.
type Marks = (
    Option<Extension<OneTime>>,
    Option<Extension<TokenPool>>,
    Option<Extension<CertifiedHost>>,
);

/// The agent routes. The caller merges them into the server's router; they have no
/// fallback of their own.
pub(crate) fn router(agents: Arc<Agents>) -> Router {
    let hello = post(hello).route_layer(from_fn_with_state(agents.clone(), limit::limit_hello));
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

/// Marks a request that was authenticated with one of a pool's shared join tokens: the name
/// of that pool, which a hello must name ([`serve_hello_in`]).
#[derive(Clone)]
struct TokenPool(String);

/// Refuses a request that does not carry one of the join tokens, a one-time token or a host
/// credential, with a 401 that says nothing else, and passes the credential the registry
/// knows the host by on (and the mark [`OneTime`] when it is a one-time token or a host
/// credential). A retired token is not known, so it gets the same 401, as does the client
/// token of the lease routes. A request whose connection carries a client certificate is
/// authenticated by it first, and its bearer is not read ([`host_cert::admit`]).
async fn authenticate(
    State(agents): State<Arc<Agents>>,
    mut request: Request,
    next: Next,
) -> Response {
    if host_cert::admit(&mut request) {
        return next.run(request).await;
    }
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    let pooled = given.and_then(|given| agents.identify_pooled(given));
    let single = match (&pooled, given) {
        (None, Some(given)) => identify_single(&agents, given).await,
        _ => Ok(None),
    };
    match (pooled, single) {
        (Some((credential, pool)), _) => {
            request.extensions_mut().insert(credential);
            request.extensions_mut().insert(TokenPool(pool));
            next.run(request).await
        }
        (None, Ok(Some(presented))) => {
            let registered = agents.joins.registered_as(&presented);
            request.extensions_mut().insert(registered);
            request.extensions_mut().insert(OneTime(presented));
            next.run(request).await
        }
        (None, unknown) => unknown_bearer(unknown),
    }
}

async fn hello(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    (one_time, pool, certified): Marks,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Refused> {
    let presented = one_time.map(|Extension(OneTime(presented))| presented);
    let pool = pool.map(|Extension(TokenPool(pool))| pool);
    let certified = certified.map(|Extension(certified)| certified);
    let given = (who, presented, pool.as_deref(), certified.as_ref());
    let welcome = serve_hello_in(&agents, given, body, &|| ()).await?;
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
#[cfg(test)]
async fn serve_hello(
    agents: &Arc<Agents>,
    who: Credential,
    one_time: Option<Credential>,
    body: Result<Bytes, BytesRejection>,
    between: &(dyn Fn() + Sync),
) -> Result<Json<Welcome>, Refused> {
    serve_hello_in(agents, (who, one_time, None, None), body, between).await
}

/// [`serve_hello`], for a request authenticated with a pool's shared join token when `pool`
/// is that pool's name: the hello must name it. On a server of several pools a hello with a
/// one-time token or a host credential must name the pool its instance's provider names.
/// Either refusal is `unknown-pool`, before anything is taken or written: a token of one
/// pool never registers a host into another. A hello with a host certificate (`certified`)
/// must name the certificate's host, is admitted into the pool it names, and may move a host
/// that is not live onto the certificate's credential ([`host_cert`]).
async fn serve_hello_in(
    agents: &Arc<Agents>,
    given: (
        Credential,
        Option<Credential>,
        Option<&str>,
        Option<&CertifiedHost>,
    ),
    body: Result<Bytes, BytesRejection>,
    between: &(dyn Fn() + Sync),
) -> Result<Json<Welcome>, Refused> {
    let (who, one_time, pool, certified) = given;
    let hello: Hello = decode(&read(body, refusal::INVALID_HELLO)?, refusal::INVALID_HELLO)?;
    hello
        .validate()
        .map_err(|err| Refused::of(&Refusal::Invalid(err)))?;
    host_cert::check(certified, &hello.host_id)?;
    let several = !agents.more_tokens.is_empty();
    let launched = hello
        .instance
        .as_ref()
        .map(|instance| instance.provider.as_str());
    let named = pool.or(launched.filter(|_| several && one_time.is_some()));
    if named.is_some_and(|named| named != hello.pool) {
        warn!(host = %hello.host_id, pool = %hello.pool, "agent hello refused: its token is another pool's");
        return Err(Refused::of(&Refusal::UnknownPool));
    }
    // Hellos are taken one at a time, from the use of the token to the registry's answer
    // and the release of a refused one, so no other hello of the host comes in between.
    let gate = agents.host_writes.lock().await;
    // The binding the registry will hold once it admits the hello. With the store, when the
    // store does not hold it yet, it is committed before the registry is asked: with the use
    // of the token in one transaction, or alone for a pool token. A known host's binding is
    // held already, and nothing is written for it.
    let host = HostId::new(&hello.host_id).map_err(Refusal::BadHostId)?;
    let restored = rows::refresh(agents, &host).await;
    restored.map_err(|error| unread_binding(&hello.host_id, &error))?;
    let change = agents.hello_change(&host, &hello, (&who, certified.is_some()));
    let writes = change.writes.clone();
    let (fresh_use, issued) = take_hello(agents, one_time.as_ref(), &hello, writes).await?;
    agents.applied(&change);
    between();
    let mut welcome = match admit_hello(agents, certified.is_some(), &hello, &who) {
        Ok(welcome) => welcome,
        Err(refusal) => {
            give_back(agents, fresh_use, &hello.host_id, &change).await;
            warn!(
                code = refusal.code(),
                reason = refusal.reason(),
                host = %hello.host_id,
                "agent hello refused: {refusal}"
            );
            return Err(Refused::of(&refusal));
        }
    };
    if let Err(refused) = rows::hello_row(agents, &host, &hello, &mut welcome).await {
        give_back(agents, fresh_use, &hello.host_id, &change).await;
        return Err(refused);
    }
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

/// The refusal of a hello whose host's stored binding could not be read: 503
/// `state-write-failed`, which the agent retries.
fn unread_binding(host: &str, error: &io::Error) -> Refused {
    warn!(%error, %host, "agent hello refused: the host's binding could not be read");
    Refused::new(StatusCode::SERVICE_UNAVAILABLE, STATE_WRITE_FAILED)
}

/// What a hello's token takes before the registry is asked, with `writes` (the host's
/// binding, with the store) in the same commit: the use that was fresh, to give back if the
/// registry refuses, and the host credential issued. One refusal for every failure, so
/// that [`serve_hello`] has no arm that only a failed commit takes: every test binary runs
/// the same code of it.
///
/// A one-time token registers the host that names its instance, and only that host
/// thereafter. The token is consumed before the registry is asked, so that two hellos
/// cannot both pass; a hello the registry then refuses (the pool is full, the host id is
/// bound to another credential) gives the use back, and the token stays unused for its
/// instance. A hello that says it keeps a host credential, and registers its host with an
/// unused token, retires the token in the same step and is answered with the credential:
/// the token alone opens nothing from then on. When the registry refuses that hello the
/// credential goes back with the use, since the answer that carries it is not sent. The
/// pool's token commits `writes` alone.
async fn take_hello(
    agents: &Arc<Agents>,
    one_time: Option<&Credential>,
    hello: &Hello,
    writes: Writes,
) -> Result<(Option<Credential>, Option<HostCredential>), Refused> {
    let Some(presented) = one_time else {
        let committed = agents.commit_hosts(writes).await;
        committed.map_err(|error| {
            warn!(%error, host = %hello.host_id, "agent hello refused: the binding could not be committed");
            Refused::new(StatusCode::SERVICE_UNAVAILABLE, STATE_WRITE_FAILED)
        })?;
        return Ok((None, None));
    };
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
    let fresh = (admitted.admission == Admission::Fresh).then(|| presented.clone());
    Ok((fresh, admitted.issued))
}

/// The compensation of a hello the registry refused after [`take_hello`]: the fresh use of
/// the token, when there was one, and the host's binding `change` go back in one
/// transaction. When it fails both stay, as the store holds them. Apart from
/// [`serve_hello`], so that the route has no arm that only one kind of token takes: every
/// test binary runs the same code of it.
async fn give_back(
    agents: &Arc<Agents>,
    fresh_use: Option<Credential>,
    host: &str,
    change: &Change,
) {
    let undo = change.undo.clone();
    let given_back = match fresh_use {
        Some(presented) => agents.release_join(presented, host.to_owned(), undo).await,
        None => agents.commit_hosts(undo).await.is_ok(),
    };
    if given_back {
        agents.undone(change);
    }
}

async fn heartbeat(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    (one_time, certified): (Option<Extension<OneTime>>, Option<Extension<CertifiedHost>>),
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<HeartbeatReply>, Refused> {
    let heartbeat: Heartbeat = decode(
        &read(body, refusal::INVALID_HEARTBEAT)?,
        refusal::INVALID_HEARTBEAT,
    )?;
    heartbeat
        .validate()
        .map_err(|err| Refused::of(&Refusal::Invalid(err)))?;
    let certified = certified.map(|Extension(certified)| certified);
    host_cert::check(certified.as_ref(), &heartbeat.host_id)?;
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
    // A beat of an epoch this server's registry does not hold may be of a session another
    // server's hello began: the registry takes it from the host's row first ([`rows`]).
    let refreshed = rows::refresh_for_beat(&agents, &heartbeat).await;
    refreshed.map_err(|error| unread_binding(&heartbeat.host_id, &error))?;
    // The reply carries the starts, stops and cancels for this host and the verdict on
    // each result, and acknowledges a taint report. The agent stops reporting once it is
    // acknowledged, so the report must be durable first. With the store, a taint the beat
    // brings is committed before the registry records it, and a failed commit answers 503
    // `state-write-failed` with nothing recorded, so the agent reports again; in file mode
    // the file is written after, and when it cannot be the reply does not acknowledge it.
    let mut reply = beat_durably(&agents, &heartbeat, &who).await?;
    let applied = rows::beat_row(&agents, &heartbeat).await?;
    // A start refused as host-tainted taints the host too, with nothing to acknowledge
    // there: the file is made current and a failure is retried by the next tick.
    let refused = heartbeat
        .results
        .iter()
        .any(|result| result.detail.as_deref() == Some(refusal::HOST_TAINTED));
    if reply.taint_ack || reply.taint_cleared_ack {
        let saved = agents.save_bindings().await;
        reply.taint_ack &= saved;
        reply.taint_cleared_ack &= saved;
    } else if refused {
        agents.save_bindings().await;
    }
    let logs = uploading(&agents, &heartbeat.host_id);
    let hold = add_jobs(&agents, (&heartbeat, applied), logs.clone(), &mut reply).await?;
    reply.log_grant =
        logs.and_then(|logs| logs.on_beat(&heartbeat.host_id, heartbeat.log_grant_state));
    if let Some(usage) = &agents.usage {
        let usage = Arc::clone(usage);
        blocking(move || usage.on_beat(&heartbeat, load)).await;
    }
    // Everything the beat brought is committed and counted; a reply with nothing to send to
    // a host that takes held replies waits here for a placement on the host.
    if let Some(hold) = hold {
        held::hold(&agents, hold, &mut reply).await;
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
/// server holds. The starts carry a log sink when `logs` is given ([`uploading`]). Returns
/// what holding the reply needs ([`held::Hold`]) when the host also reported
/// [`HELD_BEAT_CAPABILITY`].
///
/// [`PER_JOB_CAPABILITY`]: js_agent_wire::PER_JOB_CAPABILITY
/// [`HELD_BEAT_CAPABILITY`]: js_agent_wire::HELD_BEAT_CAPABILITY
async fn add_jobs(
    agents: &Arc<Agents>,
    (heartbeat, applied): (&Heartbeat, Option<NodeRow>),
    logs: Option<Arc<LogGrants>>,
    reply: &mut HeartbeatReply,
) -> Result<Option<held::Hold>, Refused> {
    let Some(jobs) = &agents.jobs else {
        return Ok(None);
    };
    let room = js_agent_wire::limits::MAX_COMMANDS.saturating_sub(reply.commands.len());
    let beat = (heartbeat, applied);
    let Some((served, watched)) = serve_jobs(agents, jobs, beat, logs, room).await? else {
        return Ok(None);
    };
    reply.commands.extend(served.commands);
    reply.job_acks = served.job_acks;
    reply.retired_acks = served.retired_acks;
    let registry_exclusive = reply.exclusive.take();
    reply.exclusive = registry_exclusive.clone().or(served.exclusive);
    Ok(watched.map(|(watch, inputs)| held::Hold {
        watch,
        inputs,
        registry_exclusive,
    }))
}

/// What the registry holds of a host that reported the `per-job` capability, as
/// [`serve_jobs`] reads it.
struct PerJobHost {
    id: HostId,
    /// The shape the host announced at its hello.
    claim: Shape,
    /// What the registry books for the host: what a start of the whole host books.
    booked: Resources,
    /// Whether the registry holds the host tainted.
    tainted: bool,
}

/// The per-job half of the heartbeat of a host that reported [`PER_JOB_CAPABILITY`], with
/// room for `room` commands, its starts carrying the sinks of `logs`, and, for a host that
/// reported [`HELD_BEAT_CAPABILITY`], the watch of its heartbeat (taken before the store is
/// read, so no placement after the read is missed) with what building the reply again needs;
/// `None` for any other host. The whole-host jobs placed on the host are served first ([`job_whole::serve`]: a
/// ready report, a refusal, a cancel, a hello), then the listing is committed and the reply
/// built ([`per_job::serve`]). The store is read and written on the pool for blocking calls.
/// A failed read or commit is a 500 `state-write-failed`. A host a refused prepare
/// tainted is written to the state file before the reply.
///
/// [`PER_JOB_CAPABILITY`]: js_agent_wire::PER_JOB_CAPABILITY
/// [`HELD_BEAT_CAPABILITY`]: js_agent_wire::HELD_BEAT_CAPABILITY
async fn serve_jobs(
    agents: &Arc<Agents>,
    jobs: &Arc<ServerJobs>,
    (heartbeat, applied): (&Heartbeat, Option<NodeRow>),
    logs: Option<Arc<LogGrants>>,
    room: usize,
) -> Result<Option<(JobReply, Option<Watched>)>, Refused> {
    let per_job = HostId::new(&heartbeat.host_id)
        .ok()
        .and_then(|host| agents.registry.own_host(&host))
        .and_then(|own| {
            let tainted = own.host.taint.is_some();
            let booked = own.host.shape.resources().clone();
            own.host.claim.map(|claim| PerJobHost {
                id: own.host.id,
                claim,
                booked,
                tainted,
            })
        })
        .filter(|host| {
            let capabilities = host.claim.capabilities();
            let reported = capabilities.get(js_agent_wire::PER_JOB_CAPABILITY);
            reported.map(String::as_str) == Some("true")
        });
    let Some(PerJobHost {
        id: host,
        claim,
        booked,
        tainted,
    }) = per_job
    else {
        return Ok(None);
    };
    let capabilities = claim.capabilities();
    // Only a serving host takes a new start: a draining, preparing or tainted one is never
    // held (a draining agent counts its drain in the waits it asks for, which a held reply
    // would stretch). A beat the row did not apply is answered at once, from the rows.
    let holds = reports_held_beat(capabilities) && heartbeat.phase == Phase::Serving;
    let holds = holds && applied.is_some();
    let watch = holds.then(|| agents.wakes.watch(&host));
    let inputs = Arc::new(held::Inputs {
        jobs: Arc::clone(jobs),
        ceiling: js_agent_wire::NetworkCeiling::from_capabilities(capabilities).unwrap_or_default(),
        host_jobs: reports_host_jobs(capabilities),
        layered_rootfs: reports_layered_rootfs(capabilities),
        mount_providers: reported_mount_providers(capabilities),
        booked,
        logs,
        listed: heartbeat.jobs.clone(),
        retired: heartbeat.retired_jobs.clone(),
        room,
        host,
    });
    let (phase, ready) = (heartbeat.phase, heartbeat.exclusive_ready.clone());
    let refused = heartbeat.exclusive_refused.clone();
    let sequenced = heartbeat.seq.is_some();
    let placing = Arc::clone(agents);
    let kept = Arc::clone(&inputs);
    let served = blocking(move || {
        let metrics = placing.meter().sink();
        let context = inputs.context(metrics);
        let (listed, retired) = (&inputs.listed, &inputs.retired);
        let Some(row) = applied else {
            // A beat the row did not apply changes nothing and is answered from the rows.
            let answer = rows_answer(&inputs.jobs, &context, (listed, retired), inputs.room);
            return (answer.map(|answer| (answer, false)), Vec::new());
        };
        // Every write of the listing is conditional on the version of the row this beat
        // wrote: once a newer beat or hello of the host moved it, the rest of this listing
        // changes nothing ([`js_jobs::JobStore::guarded`]).
        let guarded = inputs.jobs.guarded(&inputs.host, row.version);
        let (jobs, host) = (&guarded, &inputs.host);
        let first = row.record.state.first_listing_pending;
        let missed = (!sequenced).then_some(&row.record.state.missing_in_a_row);
        // The first listing after a hello loses the jobs that ran and are not listed: their
        // container went with the agent (design section 6.7, step 6). The listing's commit
        // below then frees their allocations, since the host no longer lists them.
        let held: BTreeSet<&str> = listed
            .iter()
            .map(|job| job.job_id.as_str())
            .chain(retired.iter().map(String::as_str))
            .collect();
        let lost = match first {
            true => job_loss::lose_unlisted(jobs, host, &held, metrics).map(drop),
            false => Ok(()),
        };
        // A whole-host job the host prepares for is moved before the listing is committed
        // (design section 5.4): ready starts it, a refusal fails the prepare, a hello
        // restarts it, and a cancel ends it once the agent has left `preparing`.
        let beat = job_whole::Beat {
            host,
            phase,
            ready: ready.as_deref(),
            refused: refused.as_ref(),
            first,
            tainted,
        };
        let rules = placing.whole_rules();
        let taint = |host: &HostId| placing.registry.taint_after_prepare(host);
        let serve = |()| job_whole::serve(jobs, &beat, &rules, (metrics, &taint));
        let whole = lost.and_then(serve);
        let served = whole.and_then(|taint_changed| {
            let served = per_job::serve(jobs, &context, listed, retired, inputs.room, missed);
            served.map(|served| (served, taint_changed))
        });
        // Whether the listing was clean is written to the row before the pass, so a host that
        // lists a job it holds no live allocation for takes no start until it lists only its
        // own (design section 8, row 37), on whichever server places.
        if let Ok((served, _)) = &served {
            rows::settle(&inputs.jobs, &row, served.clean, served.missing.clone());
        }
        // A listing a newer beat or hello overtook is answered from the rows as they stand.
        let served = match served {
            Err(JobStoreError::NodeMoved { .. }) => {
                let answer = rows_answer(&inputs.jobs, &context, (listed, retired), inputs.room);
                answer.map(|answer| (answer, false))
            }
            served => served,
        };
        // What the listing ended or freed, and the load the beat brought, can make room
        // for a queued job: a pass places it, and its start rides on the host's next reply,
        // or on this one when it is held (the placement wakes it). A beat that changed
        // nothing the pass depends on runs none. A host job the pass placed has tainted its
        // host: the taint is kept below.
        let placed_taints = placing.place_jobs_after_beat();
        (served, placed_taints)
    })
    .await;
    let (served, placed_taints) = served;
    agents.keep_taints(placed_taints).await;
    let host = &kept.host;
    match served {
        Ok((served, taint_changed)) => {
            if taint_changed {
                agents.save_bindings().await;
            }
            Ok(Some((served, watch.map(|watch| (watch, kept)))))
        }
        Err(error) => {
            agents.meter().sink().store_write_failed();
            warn!(%host, %error, "a heartbeat's job listing could not be committed");
            Err(Refused::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                STATE_WRITE_FAILED,
            ))
        }
    }
}

/// The per-job reply to a beat that changes nothing (the row applied a newer one first): the
/// stops and starts that follow from the rows as they stand ([`per_job::commands_now`]), with
/// no verdict on a result and no retired id answered, which the agent then reports again.
fn rows_answer(
    jobs: &ServerJobs,
    context: &per_job::BeatContext<'_>,
    listing: (&[js_agent_wire::HeldJob], &[String]),
    room: usize,
) -> Result<JobReply, js_jobs::JobStoreError> {
    let (commands, exclusive) =
        per_job::commands_now(jobs, context, listing, &BTreeSet::new(), room)?;
    Ok(JobReply {
        commands,
        job_acks: Vec::new(),
        retired_acks: Vec::new(),
        clean: true,
        exclusive,
        missing: BTreeMap::new(),
    })
}

/// The registry's reply to `heartbeat`, with the taint it brings committed first
/// ([`Agents::durable_taint`]) and the commit's guard held until the registry has recorded
/// the beat. One refusal for a failed commit and the registry's own, so that the heartbeat
/// route has no arm that only a failed commit takes: every test binary runs the same code
/// of it.
async fn beat_durably(
    agents: &Arc<Agents>,
    heartbeat: &Heartbeat,
    who: &Credential,
) -> Result<HeartbeatReply, Refused> {
    let taint = agents
        .durable_taint(heartbeat, who)
        .await
        .map_err(|error| {
            warn!(%error, host = %heartbeat.host_id, "a heartbeat's taint could not be committed");
            Refused::new(StatusCode::SERVICE_UNAVAILABLE, STATE_WRITE_FAILED)
        })?;
    let reply = agents.registry.heartbeat(heartbeat, who);
    drop(taint);
    reply.map_err(Refused::from)
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

#[cfg(test)]
#[path = "agent_api_whole_tests.rs"]
mod whole_tests;

#[cfg(test)]
#[path = "agent_api_held_tests.rs"]
mod held_tests;

#[cfg(test)]
#[path = "agent_api_whole_restart_tests.rs"]
mod whole_restart_tests;
