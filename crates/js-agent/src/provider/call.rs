//! One call to a provider: the request each kind of [`ProviderCall`] becomes, the bound on
//! its answer, and what a gRPC status means to the lifecycle.

use std::error::Error;
use std::iter::{once, successors};
use std::time::Duration;

use js_agent_wire::MountMode;
use prost::Message;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};
use tonic_prost::ProstCodec;

use super::wire;
use crate::lifecycle::{ProviderAnswer, ProviderCall};

/// The id a run has in a provider's calls: the job's id after `js.`, so a provider that other
/// clients use too keeps this agent's runs apart from theirs.
pub(crate) fn run_name(run_id: &str) -> String {
    format!("js.{run_id}")
}

/// The source a call names: the source `index` of the attempt `attempt` of `run_id`.
fn source(agent: &str, (run_id, attempt): (&str, u64), index: u32) -> Option<wire::Source> {
    Some(wire::Source {
        agent: agent.to_owned(),
        run: run_name(run_id),
        index,
        attempt,
    })
}

/// The protocol's number for a mode.
fn mode(mode: MountMode) -> i32 {
    match mode {
        MountMode::ReadOnly => wire::MODE_READ_ONLY,
        MountMode::ReadWrite => wire::MODE_READ_WRITE,
        MountMode::CopyOnWrite => wire::MODE_COPY_ON_WRITE,
    }
}

/// Sends `call` for `agent` and answers what the provider answered, or that it did not
/// within `bound`.
pub(super) async fn run(
    channel: Channel,
    agent: String,
    call: ProviderCall,
    bound: Duration,
) -> ProviderAnswer {
    match tokio::time::timeout(bound, transact(channel, &agent, call)).await {
        Ok(answer) => answer,
        Err(_) => ProviderAnswer::Unavailable(format!("no answer within {bound:?}")),
    }
}

/// One request and its response at `path`, as the generated client of a unary call makes it.
async fn unary<Q, R>(channel: Channel, path: &'static str, request: Q) -> Result<R, Status>
where
    Q: Message + Send + Sync + 'static,
    R: Message + Default + Send + Sync + 'static,
{
    let mut grpc = Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(Into::into)
        .map_err(Status::from_error)?;
    let codec = ProstCodec::<Q, R>::default();
    let path = PathAndQuery::from_static(path);
    grpc.unary(Request::new(request), path, codec)
        .await
        .map(Response::into_inner)
}

/// The request-response exchange of `call`, with no bound of its own.
async fn transact(channel: Channel, agent: &str, call: ProviderCall) -> ProviderAnswer {
    let result: Result<Option<String>, Status> = match call {
        ProviderCall::Prepare {
            run_id,
            attempt,
            index,
            reference,
            mode: how,
        } => {
            let request = wire::PrepareRequest {
                source: source(agent, (run_id.as_str(), attempt), index),
                r#ref: reference,
                mode: mode(how),
            };
            unary::<_, wire::PrepareResponse>(channel, wire::PREPARE, request)
                .await
                .map(|_| None)
        }
        ProviderCall::Mount {
            run_id,
            attempt,
            index,
        } => {
            let request = wire::MountRequest {
                source: source(agent, (run_id.as_str(), attempt), index),
            };
            unary::<_, wire::MountResponse>(channel, wire::MOUNT, request)
                .await
                .map(|response| Some(response.path))
        }
        ProviderCall::Release {
            run_id,
            attempt,
            index,
        } => {
            let request = wire::ReleaseRequest {
                source: source(agent, (run_id.as_str(), attempt), index),
            };
            unary::<_, wire::ReleaseResponse>(channel, wire::RELEASE, request)
                .await
                .map(|_| None)
        }
        ProviderCall::Sweep { keep } => {
            // Every kept run is in `keep`, for a provider that does not read the attempts; a
            // run whose attempt is known is in `keep_attempts` too.
            let request = wire::SweepRequest {
                agent: agent.to_owned(),
                keep: keep.iter().map(|(run_id, _)| run_name(run_id)).collect(),
                keep_attempts: keep
                    .iter()
                    .filter(|(_, attempt)| *attempt > 0)
                    .map(|(run_id, attempt)| wire::KeptAttempt {
                        run: run_name(run_id),
                        attempt: *attempt,
                    })
                    .collect(),
            };
            unary::<_, wire::SweepResponse>(channel, wire::SWEEP, request)
                .await
                .map(|_| None)
        }
    };
    answer(result)
}

/// What a provider's reply is to the lifecycle: `INVALID_ARGUMENT` and `FAILED_PRECONDITION`
/// are the provider refusing the ref for good; every other failure (`UNAVAILABLE`, a
/// connection that could not be made, any other code) is a try again later.
fn answer(result: Result<Option<String>, Status>) -> ProviderAnswer {
    match result {
        Ok(done) => ProviderAnswer::Done(done),
        Err(status) => match status.code() {
            Code::InvalidArgument | Code::FailedPrecondition => {
                ProviderAnswer::Refused(status.message().to_owned())
            }
            _ => ProviderAnswer::Unavailable(why(&status)),
        },
    }
}

/// A status's message followed by the errors under it, so a connect failure says which check
/// of the socket it failed and not only "transport error".
fn why(status: &Status) -> String {
    let mut parts: Vec<String> = once(status.message().to_owned())
        .chain(successors(status.source(), |&cause| cause.source()).map(ToString::to_string))
        .collect();
    parts.dedup();
    parts.join(": ")
}
