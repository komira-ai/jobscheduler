//! The job service as the server builds it from its state and serves it on one port.
//!
//! [`from_server`] is the one place a [`JobApi`] is built from the server: it takes the one
//! job store handle the server holds and the server's clock, and then gives each call group
//! the context it needs, one builder call per group. A group task whose calls the server
//! serves adds its `with_<group>` line here when the server handles that group's context
//! needs exist. Today the read group's is (it reads host silence from the registry), the
//! submit group's (the ceiling on a job's wall limit, and admission over the registry's hosts
//! and the pool's launching providers), and the logs group's (the log store of the `[logs]`
//! table, built before the configuration is consumed). The cancel group needs nothing but the tables.
//!
//! [`port`] is what the job service's listener serves: one router in front of both
//! transports, which hands a request whose content type is `application/grpc` (with or
//! without a suffix such as `+proto`) to the gRPC service and every other request to the
//! HTTP/JSON gateway. Each of the two is behind the bearer layer of the server's clients, so
//! no request reaches a call without a client's token, whichever way it is routed.

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
use js_proto::v1::job_service_server::JobServiceServer;
use tower::Service;

use super::auth::is_grpc;
use super::{Bearer, JobApi, gateway, grpc, logs, read, submit};
use crate::admit::JobAdmission;
use crate::api::Shared;
use crate::config::Clients;

/// The job service over the server's state: its job store and its clock, with each served
/// group's context, a submitted job's wall limit held to `max_job_wall` and logs read as
/// `logs` says. `None` when the server holds no job store: the job service is off.
pub(crate) fn from_server(
    state: &Shared,
    max_job_wall: Duration,
    logs: logs::Ctx,
) -> Option<JobApi> {
    let agents = state.agents()?;
    let jobs = agents.jobs.clone()?;
    let silence: Arc<dyn read::Silence> = Arc::new(agents.registry.clone());
    let read = read::Ctx::default().with_silence(silence);
    let admission = JobAdmission::new(state.admit().clone(), agents.registry.clone());
    let submit = submit::Ctx::served(max_job_wall, Arc::new(admission));
    let api = JobApi::new(jobs, agents.clock.clone());
    Some(api.with_read(read).with_submit(submit).with_logs(logs))
}

/// The router of the job service's port: gRPC and the gateway over `api`, each behind the
/// bearer layer of `clients`, chosen by the request's content type.
pub(crate) fn port(api: JobApi, clients: Clients) -> Router {
    let api = Arc::new(api);
    let both = Both {
        grpc: grpc(Arc::clone(&api), clients.clone()),
        gateway: gateway(api, clients),
    };
    Router::new().fallback_service(both)
}

/// The two transports of the job service, as one service.
#[derive(Clone)]
struct Both {
    grpc: Bearer<JobServiceServer<JobApi>>,
    gateway: Router,
}

/// The answer of [`Both`].
type Answer = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

impl Service<Request<Body>> for Both {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Answer;

    /// Always ready: each request waits, in its answer, for the readiness of the
    /// transport it goes to.
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Answer {
        match is_grpc(request.headers()) {
            true => {
                let grpc = self.grpc.clone();
                Box::pin(async move {
                    let answer = when_ready(grpc, request).await?;
                    Ok::<_, Infallible>(answer.map(Body::new))
                })
            }
            false => Box::pin(when_ready(self.gateway.clone(), request)),
        }
    }
}

/// Calls `service` with `request` once it is ready.
async fn when_ready<S, R>(mut service: S, request: R) -> Result<S::Response, S::Error>
where
    S: Service<R>,
{
    poll_fn(|cx| service.poll_ready(cx)).await?;
    service.call(request).await
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
