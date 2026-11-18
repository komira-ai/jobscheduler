//! The protocol a node-local mount provider serves (`jobscheduler.provider.v1`): the bytes of
//! every message against `golden/provider_messages.hex`, which a provider built elsewhere is
//! held to, the numbers of the mount mode, the five calls and their paths, and the generated
//! client and server run against each other in one process over an in-memory duplex stream: no
//! TCP or UDP socket and no port.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::future::{Ready, ready};
use std::io;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use js_proto::provider::v1::mount_provider_client::MountProviderClient;
use js_proto::provider::v1::mount_provider_server::{
    MountProvider, MountProviderServer, SERVICE_NAME,
};
use js_proto::provider::v1::{
    CheckRequest, CheckResponse, MountMode, MountRequest, MountResponse, PrepareRequest,
    PrepareResponse, ReleaseRequest, ReleaseResponse, Source, SweepRequest, SweepResponse,
};
use prost::Message;
use prost::encoding::{WireType, encode_key, encode_varint};
use tokio::io::DuplexStream;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::codegen::tokio_stream;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Request, Response, Status};
use tonic_prost::ProstCodec;

/// A connector that hands the channel one end of an in-memory stream, once.
struct InMemory(Option<DuplexStream>);

impl Service<Uri> for InMemory {
    type Response = TokioIo<DuplexStream>;
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        ready(
            self.0
                .take()
                .map(TokioIo::new)
                .ok_or_else(|| io::Error::other("the in-memory stream connects once")),
        )
    }
}

/// A channel to `service`, served on a task of this runtime over an in-memory stream. The
/// endpoint's URI names no host that exists and is never resolved: the connector ignores it.
async fn connect(service: impl MountProvider) -> Channel {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let server = Server::builder().serve_with_incoming(
        MountProviderServer::new(service),
        tokio_stream::once(Ok::<_, io::Error>(server_end)),
    );
    tokio::spawn(server);
    Endpoint::from_static("http://in-process.invalid")
        .connect_with_connector(InMemory(Some(client_end)))
        .await
        .expect("the in-memory channel connects")
}

/// The golden: each message's encoding by the name of its line.
fn golden() -> BTreeMap<&'static str, Vec<u8>> {
    include_str!("golden/provider_messages.hex")
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, hex) = line.split_once(':').expect("a line is `Name: bytes`");
            let bytes = hex
                .split_whitespace()
                .map(|pair| {
                    assert_eq!(pair.len(), 2, "{pair:?} is not one byte in hex");
                    u8::from_str_radix(pair, 16).expect("a hex pair")
                })
                .collect();
            (name, bytes)
        })
        .collect()
}

/// `bytes` with four fields appended that no message of this version has, one of each wire
/// type a new field can have, numbered far above any number in use.
fn with_unknown_fields(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    encode_key(100_000, WireType::Varint, &mut out);
    encode_varint(42, &mut out);
    encode_key(100_001, WireType::LengthDelimited, &mut out);
    encode_varint(3, &mut out);
    out.extend_from_slice(b"new");
    encode_key(100_002, WireType::ThirtyTwoBit, &mut out);
    out.extend_from_slice(&7_u32.to_le_bytes());
    encode_key(100_003, WireType::SixtyFourBit, &mut out);
    out.extend_from_slice(&u64::MAX.to_le_bytes());
    out
}

/// Asserts that `message` encodes to the golden line `name`, that those bytes decode to
/// `message`, and that they decode to it still with a field of every wire type this version
/// does not know appended.
fn holds<M: Message + Default + PartialEq + Debug>(
    golden: &BTreeMap<&'static str, Vec<u8>>,
    name: &str,
    message: &M,
) {
    let bytes = golden
        .get(name)
        .unwrap_or_else(|| panic!("{name} is golden"));
    assert_eq!(&message.encode_to_vec(), bytes, "{name} encodes");
    let read = M::decode(bytes.as_slice()).expect("the golden decodes");
    assert_eq!(&read, message, "{name} decodes");
    let later = M::decode(with_unknown_fields(bytes).as_slice()).expect("a later reader's bytes");
    assert_eq!(&later, message, "{name} skips unknown fields");
}

/// The source every sample names: the third source of the run `js.job-1`, as the agent of
/// `host-1` names it.
fn source() -> Source {
    Source {
        agent: "host-1".to_owned(),
        run: "js.job-1".to_owned(),
        index: 2,
    }
}

fn prepare_request() -> PrepareRequest {
    PrepareRequest {
        source: Some(source()),
        r#ref: "toolchains/clang-18".to_owned(),
        mode: MountMode::CopyOnWrite as i32,
    }
}

fn mount_request() -> MountRequest {
    MountRequest {
        source: Some(source()),
    }
}

fn mount_response() -> MountResponse {
    MountResponse {
        path: "/srv/tier/job-1/2".to_owned(),
    }
}

fn release_request() -> ReleaseRequest {
    ReleaseRequest {
        source: Some(source()),
    }
}

fn check_request() -> CheckRequest {
    CheckRequest {
        source: Some(source()),
    }
}

fn check_response() -> CheckResponse {
    CheckResponse {
        lost: "tree-lost".to_owned(),
    }
}

fn sweep_request() -> SweepRequest {
    SweepRequest {
        agent: "host-1".to_owned(),
        keep: vec!["js.job-1".to_owned(), "js.job-2".to_owned()],
    }
}

fn sweep_response() -> SweepResponse {
    SweepResponse { released: 5 }
}

/// Asserts that a fully populated message of each kind encodes to the bytes of its line of
/// `golden/provider_messages.hex`, written by hand from the field numbers, decodes from them,
/// and decodes from them with unknown fields of every wire type appended; and that the
/// golden holds no other line. It catches a renumbered, retyped or reordered field, a field
/// that does not skip an unknown one, and a message the golden does not pin.
#[test]
fn every_message_of_the_protocol_has_its_golden_bytes() {
    let golden = golden();
    holds(&golden, "Source", &source());
    holds(&golden, "PrepareRequest", &prepare_request());
    holds(&golden, "PrepareResponse", &PrepareResponse {});
    holds(&golden, "MountRequest", &mount_request());
    holds(&golden, "MountResponse", &mount_response());
    holds(&golden, "ReleaseRequest", &release_request());
    holds(&golden, "ReleaseResponse", &ReleaseResponse {});
    holds(&golden, "CheckRequest", &check_request());
    holds(&golden, "CheckResponse", &check_response());
    holds(&golden, "SweepRequest", &sweep_request());
    holds(&golden, "SweepResponse", &sweep_response());
    assert_eq!(golden.len(), 11);
}

/// Asserts the numbers of the mount mode and how a request reads one: the four values at 0 to
/// 3, a number this version does not name kept as the raw number through a decode and an
/// encode and read as unspecified by the accessor, and an absent mode read as unspecified.
/// It catches a renumbered value and a reader that refuses or rewrites a later release's
/// mode.
#[test]
fn the_mount_mode_has_its_numbers_and_keeps_a_later_one() {
    assert_eq!(MountMode::Unspecified as i32, 0);
    assert_eq!(MountMode::ReadOnly as i32, 1);
    assert_eq!(MountMode::ReadWrite as i32, 2);
    assert_eq!(MountMode::CopyOnWrite as i32, 3);
    assert_eq!(MountMode::ReadWrite.as_str_name(), "MOUNT_MODE_READ_WRITE");
    assert_eq!(prepare_request().mode(), MountMode::CopyOnWrite);
    let later = PrepareRequest {
        mode: 99,
        ..prepare_request()
    };
    let bytes = later.encode_to_vec();
    let read = PrepareRequest::decode(bytes.as_slice()).expect("the request decodes");
    assert_eq!(read.mode, 99);
    assert_eq!(read.mode(), MountMode::Unspecified);
    assert_eq!(read.encode_to_vec(), bytes);
    let absent = PrepareRequest::decode(&b""[..]).expect("an empty request decodes");
    assert_eq!(absent.mode(), MountMode::Unspecified);
    assert_eq!(absent.source, None);
}

/// One call as the recording server received it.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Prepare(PrepareRequest),
    Mount(MountRequest),
    Release(ReleaseRequest),
    Check(CheckRequest),
    Sweep(SweepRequest),
}

/// A server that records every request and answers each method with its own sample.
#[derive(Clone, Default)]
struct Recording {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Recording {
    fn record<T>(&self, call: Call, answer: T) -> Result<Response<T>, Status> {
        self.calls
            .lock()
            .expect("the record is not poisoned")
            .push(call);
        Ok(Response::new(answer))
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .expect("the record is not poisoned")
            .clone()
    }
}

#[tonic::async_trait]
impl MountProvider for Recording {
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        self.record(Call::Prepare(request.into_inner()), PrepareResponse {})
    }

    async fn mount(
        &self,
        request: Request<MountRequest>,
    ) -> Result<Response<MountResponse>, Status> {
        self.record(Call::Mount(request.into_inner()), mount_response())
    }

    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        self.record(Call::Release(request.into_inner()), ReleaseResponse {})
    }

    async fn check(
        &self,
        request: Request<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        self.record(Call::Check(request.into_inner()), check_response())
    }

    async fn sweep(
        &self,
        request: Request<SweepRequest>,
    ) -> Result<Response<SweepResponse>, Status> {
        self.record(Call::Sweep(request.into_inner()), sweep_response())
    }
}

/// Asserts that the generated client makes each of the five calls against the generated
/// server over the in-memory stream: the server receives each request whole and in order, and
/// the client receives each answer whole. It catches a call whose request or answer is lost,
/// altered or routed to another method on the way.
#[tokio::test]
async fn the_five_calls_round_trip_between_the_client_and_the_server() {
    let server = Recording::default();
    let mut client = MountProviderClient::new(connect(server.clone()).await);
    let prepared = client.prepare(prepare_request()).await.expect("prepare");
    assert_eq!(prepared.into_inner(), PrepareResponse {});
    let mounted = client.mount(mount_request()).await.expect("mount");
    assert_eq!(mounted.into_inner(), mount_response());
    let released = client.release(release_request()).await.expect("release");
    assert_eq!(released.into_inner(), ReleaseResponse {});
    let checked = client.check(check_request()).await.expect("check");
    assert_eq!(checked.into_inner(), check_response());
    let swept = client.sweep(sweep_request()).await.expect("sweep");
    assert_eq!(swept.into_inner(), sweep_response());
    assert_eq!(
        server.calls(),
        [
            Call::Prepare(prepare_request()),
            Call::Mount(mount_request()),
            Call::Release(release_request()),
            Call::Check(check_request()),
            Call::Sweep(sweep_request()),
        ]
    );
}

/// Sends `message` to `method` of the service with the generic client, a path of this test's
/// own and not the generated client's, and returns the answer.
async fn raw<M, R>(channel: Channel, method: &str, message: M) -> Result<R, Status>
where
    M: Message + Default + Send + Sync + 'static,
    R: Message + Default + Send + Sync + 'static,
{
    let path = PathAndQuery::try_from(format!("/jobscheduler.provider.v1.MountProvider/{method}"))
        .expect("a path");
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.expect("the channel is ready");
    let answer = grpc
        .unary(Request::new(message), path, ProstCodec::<M, R>::default())
        .await?;
    Ok(answer.into_inner())
}

/// Asserts the name of the service and the paths of its five calls: a request to
/// `/jobscheduler.provider.v1.MountProvider/<Method>` with the golden's message reaches the
/// method of that name and is answered with the message the golden holds, and a path of
/// another name is refused `UNIMPLEMENTED`. It catches a renamed service, package or method,
/// which a provider built elsewhere would meet as a call that goes nowhere.
#[tokio::test]
async fn the_calls_have_the_paths_a_provider_serves() {
    assert_eq!(SERVICE_NAME, "jobscheduler.provider.v1.MountProvider");
    let server = Recording::default();
    let channel = connect(server.clone()).await;
    let answer: PrepareResponse = raw(channel.clone(), "Prepare", prepare_request())
        .await
        .expect("prepare");
    assert_eq!(answer, PrepareResponse {});
    let answer: MountResponse = raw(channel.clone(), "Mount", mount_request())
        .await
        .expect("mount");
    assert_eq!(answer, mount_response());
    let answer: ReleaseResponse = raw(channel.clone(), "Release", release_request())
        .await
        .expect("release");
    assert_eq!(answer, ReleaseResponse {});
    let answer: CheckResponse = raw(channel.clone(), "Check", check_request())
        .await
        .expect("check");
    assert_eq!(answer, check_response());
    let answer: SweepResponse = raw(channel.clone(), "Sweep", sweep_request())
        .await
        .expect("sweep");
    assert_eq!(answer, sweep_response());
    assert_eq!(
        server.calls(),
        [
            Call::Prepare(prepare_request()),
            Call::Mount(mount_request()),
            Call::Release(release_request()),
            Call::Check(check_request()),
            Call::Sweep(sweep_request()),
        ]
    );
    let refused: Result<SweepResponse, Status> = raw(channel, "Sweeps", sweep_request()).await;
    assert_eq!(
        refused.expect_err("no such method").code(),
        Code::Unimplemented
    );
}

/// A server that refuses every call with a status of its own.
struct Refusing;

#[tonic::async_trait]
impl MountProvider for Refusing {
    async fn prepare(
        &self,
        _: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        Err(Status::invalid_argument("the ref names no tree"))
    }

    async fn mount(&self, _: Request<MountRequest>) -> Result<Response<MountResponse>, Status> {
        Err(Status::failed_precondition("the tree will never be ready"))
    }

    async fn release(
        &self,
        _: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        Err(Status::unavailable("the provider is draining"))
    }

    async fn check(&self, _: Request<CheckRequest>) -> Result<Response<CheckResponse>, Status> {
        Err(Status::deadline_exceeded("the provider is slow"))
    }

    async fn sweep(&self, _: Request<SweepRequest>) -> Result<Response<SweepResponse>, Status> {
        Err(Status::internal("the collection is unreadable"))
    }
}

/// Asserts that a provider's refusal reaches the client with its code and its message on
/// every call: `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `UNAVAILABLE`, `DEADLINE_EXCEEDED` and
/// `INTERNAL`. It
/// catches a status that the transport changes on the way, which the agent maps to a refusal
/// of the job or a try again.
#[tokio::test]
async fn a_providers_status_reaches_the_client_whole() {
    let mut client = MountProviderClient::new(connect(Refusing).await);
    let prepare = client
        .prepare(prepare_request())
        .await
        .expect_err("refused");
    assert_eq!(prepare.code(), Code::InvalidArgument);
    assert_eq!(prepare.message(), "the ref names no tree");
    let mount = client.mount(mount_request()).await.expect_err("refused");
    assert_eq!(mount.code(), Code::FailedPrecondition);
    assert_eq!(mount.message(), "the tree will never be ready");
    let release = client
        .release(release_request())
        .await
        .expect_err("refused");
    assert_eq!(release.code(), Code::Unavailable);
    assert_eq!(release.message(), "the provider is draining");
    let check = client.check(check_request()).await.expect_err("refused");
    assert_eq!(check.code(), Code::DeadlineExceeded);
    assert_eq!(check.message(), "the provider is slow");
    let sweep = client.sweep(sweep_request()).await.expect_err("refused");
    assert_eq!(sweep.code(), Code::Internal);
    assert_eq!(sweep.message(), "the collection is unreadable");
}

/// A server that implements one method and leaves the rest as generated.
struct ReleaseOnly;

#[tonic::async_trait]
impl MountProvider for ReleaseOnly {
    async fn release(
        &self,
        _: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        Ok(Response::new(ReleaseResponse {}))
    }
}

/// Asserts that a server that implements only `Release` answers `Release` and each of the other
/// four calls (`Prepare`, `Mount`, `Sweep` and `Check`) `UNIMPLEMENTED`, which is how a provider
/// that has not grown a later method behaves. It catches a build that leaves a method out of the stubs, so
/// that a partial provider would not compile or would answer another code.
#[tokio::test]
async fn a_method_a_server_leaves_out_is_unimplemented() {
    let mut client = MountProviderClient::new(connect(ReleaseOnly).await);
    let prepare = client
        .prepare(prepare_request())
        .await
        .expect_err("unimplemented");
    assert_eq!(prepare.code(), Code::Unimplemented);
    let mount = client
        .mount(mount_request())
        .await
        .expect_err("unimplemented");
    assert_eq!(mount.code(), Code::Unimplemented);
    let check = client
        .check(check_request())
        .await
        .expect_err("unimplemented");
    assert_eq!(check.code(), Code::Unimplemented);
    let sweep = client
        .sweep(sweep_request())
        .await
        .expect_err("unimplemented");
    assert_eq!(sweep.code(), Code::Unimplemented);
    let released = client
        .release(release_request())
        .await
        .expect("implemented");
    assert_eq!(released.into_inner(), ReleaseResponse {});
}
