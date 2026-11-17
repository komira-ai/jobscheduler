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
    /// A connector to `socket`, which must belong to `euid`, to root or to its directory's
    /// owner ([`check_socket`]).
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

/// Fails unless `path` is a socket that neither group nor others can write, in a directory
/// that neither group nor others can write, and that belongs to `euid`, to root or to the
/// owner of that directory (a provider that runs as a user of its own, such as tierstore,
/// binds its socket in a directory of its own): whoever may write the file, or replace it in
/// its directory, could stand in for the provider, and the agent would hand it the refs of
/// jobs and trust the directories it answers.
fn check_socket(path: &Path, euid: u32) -> io::Result<()> {
    let meta = std::fs::metadata(path)
        .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", path.display())))?;
    let dir = path.parent().unwrap_or(path);
    // A directory that cannot be read is judged as one others may write.
    let (dir_uid, dir_mode) =
        std::fs::metadata(dir).map_or((None, OTHERS_WRITE), |dir| (Some(dir.uid()), dir.mode()));
    let found = Found {
        socket: meta.file_type().is_socket(),
        uid: meta.uid(),
        mode: meta.mode(),
        dir_uid,
        dir_mode,
    };
    judge(&found, euid).map_err(|why| io::Error::other(format!("{}: {why}", path.display())))
}

/// What [`check_socket`] read of a socket file and its directory.
#[derive(Debug, Clone, Copy)]
struct Found {
    /// Whether the file is a socket.
    socket: bool,
    /// The file's owner.
    uid: u32,
    /// The file's mode.
    mode: u32,
    /// The directory's owner, when it could be read.
    dir_uid: Option<u32>,
    /// The directory's mode.
    dir_mode: u32,
}

/// The rules of [`check_socket`] over what it read, for an agent running as `euid`.
fn judge(found: &Found, euid: u32) -> Result<(), String> {
    if !found.socket {
        return Err("not a socket".to_owned());
    }
    if found.dir_mode & OTHERS_WRITE != 0 {
        return Err(format!(
            "its directory can be written by its group or by others (mode {:o})",
            found.dir_mode & 0o7777
        ));
    }
    if ![Some(euid), Some(0), found.dir_uid].contains(&Some(found.uid)) {
        return Err(format!(
            "belongs to user {}, neither this agent's user ({euid}), root nor its directory's owner",
            found.uid
        ));
    }
    if found.mode & OTHERS_WRITE != 0 {
        return Err(format!(
            "can be written by its group or by others (mode {:o})",
            found.mode & 0o7777
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Found, judge};

    /// A socket of user 7, mode 0600, in a directory of user 7, mode 0755.
    fn good() -> Found {
        Found {
            socket: true,
            uid: 7,
            mode: 0o140_600,
            dir_uid: Some(7),
            dir_mode: 0o040_755,
        }
    }

    /// Asserts that a socket is taken when it belongs to the agent's user, to root or to its
    /// directory's owner, and refused, naming the rule, when it is not a socket, when its
    /// directory or the socket can be written by group or others, or when it belongs to
    /// another user. Catches a provider's socket in its own directory refused, and a socket
    /// a third user could have planted taken.
    #[test]
    fn a_socket_is_taken_only_from_a_trusted_owner_in_a_closed_directory() {
        assert_eq!(judge(&good(), 9), Ok(()), "the directory's owner");
        let of_agent = Found {
            dir_uid: Some(0),
            ..good()
        };
        assert_eq!(judge(&of_agent, 7), Ok(()), "the agent's user");
        let of_root = Found {
            uid: 0,
            dir_uid: Some(3),
            ..good()
        };
        assert_eq!(judge(&of_root, 9), Ok(()), "root");
        let refused = [
            (
                Found {
                    socket: false,
                    ..good()
                },
                "not a socket".to_owned(),
            ),
            (
                Found {
                    dir_mode: 0o040_775,
                    ..good()
                },
                "its directory can be written by its group or by others (mode 775)".to_owned(),
            ),
            (
                Found {
                    dir_mode: 0o041_777,
                    ..good()
                },
                "its directory can be written by its group or by others (mode 1777)".to_owned(),
            ),
            (
                Found {
                    dir_uid: Some(3),
                    ..good()
                },
                "belongs to user 7, neither this agent's user (9), root nor its directory's owner"
                    .to_owned(),
            ),
            (
                Found {
                    dir_uid: None,
                    ..good()
                },
                "belongs to user 7, neither this agent's user (9), root nor its directory's owner"
                    .to_owned(),
            ),
            (
                Found {
                    mode: 0o140_620,
                    ..good()
                },
                "can be written by its group or by others (mode 620)".to_owned(),
            ),
            (
                Found {
                    mode: 0o140_602,
                    ..good()
                },
                "can be written by its group or by others (mode 602)".to_owned(),
            ),
        ];
        for (found, why) in refused {
            assert_eq!(judge(&found, 9), Err(why), "{found:?}");
        }
    }
}
