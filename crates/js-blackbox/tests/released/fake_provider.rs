//! The fake node-local mount provider: tierstore's place beside the agent, as a program of its
//! own that serves the real protocol (`jobscheduler.provider.v1.MountProvider`) on a Unix socket.
//!
//! It answers every `Prepare` one way, chosen when it starts (`--answer`):
//!
//! - `success`: the source is prepared; `Mount` makes `<trees>/<agent>/<run>/<index>` (a
//!   directory holding one file, `ref`, with the ref it was prepared with) and answers it;
//! - `refuse`: `INVALID_ARGUMENT`, a ref the provider will never make;
//! - `unavailable`: `UNAVAILABLE`, a provider that is not ready;
//! - `full`: `UNAVAILABLE` naming a full placement limit, as a provider whose placements are all
//!   taken answers;
//! - `hold`: no answer at all, a provider that hangs.
//!
//! `Release` and `Sweep` always succeed. The black-box scenarios use `success`; the other answers
//! are the ones the agent's own provider tests start it with.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use js_proto::provider::v1::mount_provider_server::{MountProvider, MountProviderServer};
use js_proto::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
    Source, SweepRequest, SweepResponse,
};
use tokio::net::UnixListener;
use tonic::codegen::tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

/// How the fake answers `Prepare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Answer {
    /// Prepared, and mounted on request.
    Success,
    /// `INVALID_ARGUMENT`.
    Refuse,
    /// `UNAVAILABLE`, not ready.
    Unavailable,
    /// `UNAVAILABLE`, the placement limit full.
    Full,
    /// Never answered.
    Hold,
}

/// The flags of `released fake-provider`.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// The Unix socket to serve on (removed first if a file is there).
    #[arg(long)]
    pub socket: PathBuf,
    /// The directory the mounted trees are made in; it must lie under a mount root of the agent.
    #[arg(long)]
    pub trees: PathBuf,
    /// How `Prepare` is answered.
    #[arg(long, value_enum)]
    pub answer: Answer,
}

fn key(source: Option<Source>) -> (String, String, u32) {
    let source = source.unwrap_or_default();
    (source.agent, source.run, source.index)
}

struct Fake {
    answer: Answer,
    trees: PathBuf,
    /// The refs of the prepared sources.
    prepared: Mutex<BTreeMap<(String, String, u32), String>>,
}

#[tonic::async_trait]
impl MountProvider for Fake {
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        match self.answer {
            Answer::Success => {
                self.prepared
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key(request.source), request.r#ref);
                Ok(Response::new(PrepareResponse {}))
            }
            Answer::Refuse => Err(Status::invalid_argument(format!(
                "the fake provider refuses the ref {}",
                request.r#ref
            ))),
            Answer::Unavailable => Err(Status::unavailable("the fake provider is not ready")),
            Answer::Full => Err(Status::unavailable(
                "the fake provider's placements are full (max_placements reached)",
            )),
            Answer::Hold => std::future::pending().await,
        }
    }

    async fn mount(
        &self,
        request: Request<MountRequest>,
    ) -> Result<Response<MountResponse>, Status> {
        let source = key(request.into_inner().source);
        let reference = self
            .prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&source)
            .cloned()
            .ok_or_else(|| Status::failed_precondition("the source was not prepared"))?;
        let dir = self
            .trees
            .join(&source.0)
            .join(&source.1)
            .join(source.2.to_string());
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join("ref"), reference))
            .map_err(|e| Status::internal(format!("make {}: {e}", dir.display())))?;
        Ok(Response::new(MountResponse {
            path: dir.display().to_string(),
        }))
    }

    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        self.prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key(request.into_inner().source));
        Ok(Response::new(ReleaseResponse {}))
    }

    async fn sweep(
        &self,
        _request: Request<SweepRequest>,
    ) -> Result<Response<SweepResponse>, Status> {
        Ok(Response::new(SweepResponse { released: 0 }))
    }
}

/// Serves until the process is stopped.
pub fn serve(flags: &Flags) {
    let _ = std::fs::remove_file(&flags.socket);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("the fake provider's runtime");
    let fake = Fake {
        answer: flags.answer,
        trees: flags.trees.clone(),
        prepared: Mutex::new(BTreeMap::new()),
    };
    runtime.block_on(async {
        let listener = UnixListener::bind(&flags.socket)
            .unwrap_or_else(|e| panic!("bind {}: {e}", flags.socket.display()));
        eprintln!(
            "fake provider: answering {:?} on {}",
            flags.answer,
            flags.socket.display()
        );
        Server::builder()
            .serve_with_incoming(
                MountProviderServer::new(fake),
                UnixListenerStream::new(listener),
            )
            .await
            .expect("the fake provider serves");
    });
}
