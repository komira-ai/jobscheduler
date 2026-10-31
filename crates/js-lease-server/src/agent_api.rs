//! The routes agents call, behind the join tokens: the hello and the heartbeat, and the
//! routes a host calls about itself ([`own_host`]).
//!
//! Every answer is a status and `{"error": "<code>"}` with a code from
//! [`js_agent_wire::refusal`], or the message the route returns. Nothing the peer sent
//! reaches an answer: a body that does not decode is answered with the route's
//! `invalid-*` code, never with the decoder's message, which repeats the text it choked
//! on.

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
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use crate::agents::Agents;
use crate::api::{BODY_LIMIT, bearer};
use crate::join_tokens::{Admission, JoinRefusal};
use crate::util::blocking;

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

/// The hello route. `between` runs after a one-time token was admitted and before the
/// registry is asked: the point at which another hello can get in. Production passes a
/// function that does nothing; a test passes the other hello, so the interleaving is
/// exact and no thread races on timing. `who` is the credential the registry knows the host
/// by; `one_time` is the digest of the one-time token or host credential the request
/// carried, when it carried one.
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
    // A one-time token registers the host that names its instance, and only that host
    // thereafter. The token is consumed before the registry is asked, so that two hellos
    // cannot both pass; a hello the registry then refuses (the pool is full, the host id
    // is bound to another credential) gives the use back, and the token stays unused for
    // its instance. Not when a later hello of the same host was admitted meanwhile: the
    // registry may have accepted that one, and the token then stays the host's.
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
            .admit_join(presented.clone(), host, instance, asked)
            .await;
        let admitted = admitted.map_err(|refusal| {
            warn!(code = refusal.code(), host = %hello.host_id, "agent hello refused: join token");
            Refused::join(refusal)
        })?;
        if admitted.admission == Admission::Fresh {
            fresh_use = Some(presented.clone());
        }
        issued = admitted.issued;
    }
    between();
    let mut welcome = match agents.registry.hello(&hello, &who) {
        Ok(welcome) => welcome,
        Err(refusal) => {
            if let Some(presented) = fresh_use {
                agents.release_join(presented, hello.host_id.clone()).await;
            }
            warn!(code = refusal.code(), "agent hello refused");
            return Err(Refused::of(&refusal));
        }
    };
    let credential_issued = issued.is_some();
    welcome.host_credential = issued;
    agents.save_bindings().await;
    info!(host = %hello.host_id, epoch = welcome.epoch, credential_issued, "agent admitted");
    Ok(Json(welcome))
}

async fn heartbeat(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    one_time: Option<Extension<OneTime>>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<HeartbeatReply>, Refused> {
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
    // acknowledged, so the report must be in the state file first: when it cannot be
    // written the reply does not acknowledge it, and the agent reports again.
    let mut reply = agents
        .registry
        .heartbeat(&heartbeat, &who)
        .map_err(|refusal| Refused::of(&refusal))?;
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
