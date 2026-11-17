//! One call to a provider: the request each kind of [`ProviderCall`] becomes, the bound on
//! its answer, and what a gRPC status means to the lifecycle.

use std::error::Error;
use std::iter::{once, successors};
use std::time::Duration;

use js_agent_wire::MountMode;
use js_proto::provider::v1 as pb;
use js_proto::provider::v1::mount_provider_client::MountProviderClient;
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::lifecycle::{ProviderAnswer, ProviderCall};

/// The id a run has in a provider's calls: the job's id after `js.`, so a provider that other
/// clients use too keeps this agent's runs apart from theirs.
fn run_name(run_id: &str) -> String {
    format!("js.{run_id}")
}

/// The source a call names.
fn source(agent: &str, run_id: &str, index: u32) -> Option<pb::Source> {
    Some(pb::Source {
        agent: agent.to_owned(),
        run: run_name(run_id),
        index,
    })
}

/// The protocol's name for a mode.
fn mode(mode: MountMode) -> pb::MountMode {
    match mode {
        MountMode::ReadOnly => pb::MountMode::ReadOnly,
        MountMode::ReadWrite => pb::MountMode::ReadWrite,
        MountMode::CopyOnWrite => pb::MountMode::CopyOnWrite,
    }
}

/// Sends `call` for `agent` and answers what the provider answered, or that it did not
/// within `bound`.
pub(super) async fn run(
    client: MountProviderClient<Channel>,
    agent: String,
    call: ProviderCall,
    bound: Duration,
) -> ProviderAnswer {
    match tokio::time::timeout(bound, transact(client, &agent, call)).await {
        Ok(answer) => answer,
        Err(_) => ProviderAnswer::Unavailable(format!("no answer within {bound:?}")),
    }
}

/// The request-response exchange of `call`, with no bound of its own.
async fn transact(
    mut client: MountProviderClient<Channel>,
    agent: &str,
    call: ProviderCall,
) -> ProviderAnswer {
    let result: Result<Option<String>, Status> = match call {
        ProviderCall::Prepare {
            run_id,
            index,
            reference,
            mode: how,
        } => client
            .prepare(pb::PrepareRequest {
                source: source(agent, &run_id, index),
                r#ref: reference,
                mode: mode(how) as i32,
                host: agent.to_owned(),
            })
            .await
            .map(|_| None),
        ProviderCall::Mount { run_id, index } => client
            .mount(pb::MountRequest {
                source: source(agent, &run_id, index),
            })
            .await
            .map(|response| Some(response.into_inner().path)),
        ProviderCall::Release { run_id, index } => client
            .release(pb::ReleaseRequest {
                source: source(agent, &run_id, index),
            })
            .await
            .map(|_| None),
        ProviderCall::Sweep { keep } => client
            .sweep(pb::SweepRequest {
                agent: agent.to_owned(),
                keep: keep.iter().map(|run_id| run_name(run_id)).collect(),
            })
            .await
            .map(|_| None),
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
        .chain(successors(status.source(), |cause| cause.source()).map(ToString::to_string))
        .collect();
    parts.dedup();
    parts.join(": ")
}
