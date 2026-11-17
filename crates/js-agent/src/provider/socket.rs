//! The connector of a provider's channel: it checks the socket file, then connects to it.
//!
//! The channel is lazy and reconnects by itself, so the connector runs for the first call and
//! again after a connection is lost, and every run checks the file again: a provider that
//! restarts (a new socket file) or a file replaced by another user is met by the same checks
//! as the first connect.

use std::future::Future;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;

/// The permission bits of a socket that let someone besides its owner write to it: group and
/// others.
const OTHERS_WRITE: u32 = 0o022;

/// Opens the Unix socket of one provider for tonic, which hands the connector a URI it has
/// no use for.
pub(super) struct UnixConnector {
    socket: PathBuf,
    euid: u32,
}

impl UnixConnector {
    /// A connector to `socket`, which must belong to `euid` or to root.
    pub(super) fn new(socket: PathBuf, euid: u32) -> Self {
        Self { socket, euid }
    }
}

impl Service<Uri> for UnixConnector {
    type Response = TokioIo<UnixStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let socket = self.socket.clone();
        let euid = self.euid;
        Box::pin(async move {
            check_socket(&socket, euid)?;
            UnixStream::connect(&socket).await.map(TokioIo::new)
        })
    }
}

/// Fails unless `path` is a socket that belongs to `euid` or to root and that neither group
/// nor others can write: whoever may write the file could stand in for the provider, and
/// the agent would hand it the refs of jobs and trust the directories it answers.
fn check_socket(path: &Path, euid: u32) -> io::Result<()> {
    let meta = std::fs::metadata(path)
        .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", path.display())))?;
    if !meta.file_type().is_socket() {
        return Err(io::Error::other(format!(
            "{} is not a socket",
            path.display()
        )));
    }
    if ![euid, 0].contains(&meta.uid()) {
        return Err(io::Error::other(format!(
            "{} belongs to user {}, neither this agent's user ({euid}) nor root",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & OTHERS_WRITE != 0 {
        return Err(io::Error::other(format!(
            "{} can be written by its group or by others (mode {:o})",
            path.display(),
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}
