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

/// The permission bits of a directory that let someone besides its owner write to it: group
/// and others.
const OTHERS_WRITE: u32 = 0o022;

/// The permission bit of a socket that lets anyone write to it.
const OTHERS_ONLY_WRITE: u32 = 0o002;

/// Opens the Unix socket of one provider for tonic, which hands the connector a URI it has
/// no use for.
pub(super) struct UnixConnector {
    socket: PathBuf,
    /// The uid the provider runs as.
    user: u32,
}

impl UnixConnector {
    /// A connector to `socket`, the socket of a provider that runs as `user`
    /// ([`check_socket`]).
    pub(super) fn new(socket: PathBuf, user: u32) -> Self {
        Self { socket, user }
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
        let user = self.user;
        Box::pin(async move {
            check_socket(&socket, user)?;
            UnixStream::connect(&socket).await.map(TokioIo::new)
        })
    }
}

/// Fails unless `path` is a socket of `user` (the provider's) or root that no other user can
/// write (its group may: the agent's user reaches it through the socket's group), in a
/// directory of root or `user` that no one else can write, so that only root or the provider
/// can have put it there. Whoever else could put a socket in its place could stand in for the
/// provider, and the agent would hand it the refs of jobs and trust the directories it
/// answers.
fn check_socket(path: &Path, user: u32) -> io::Result<()> {
    let meta = std::fs::metadata(path)
        .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", path.display())))?;
    let dir = path.parent().unwrap_or(path);
    // A directory that cannot be read is judged as one of nobody's.
    let (dir_uid, dir_mode) =
        std::fs::metadata(dir).map_or((None, OTHERS_WRITE), |dir| (Some(dir.uid()), dir.mode()));
    let found = Found {
        socket: meta.file_type().is_socket(),
        uid: meta.uid(),
        mode: meta.mode(),
        dir_uid,
        dir_mode,
    };
    judge(&found, user).map_err(|why| io::Error::other(format!("{}: {why}", path.display())))
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

/// The rules of [`check_socket`] over what it read, for a provider that runs as `user`.
fn judge(found: &Found, user: u32) -> Result<(), String> {
    if !found.socket {
        return Err("not a socket".to_owned());
    }
    if ![0, user].contains(&found.uid) {
        return Err(format!(
            "belongs to user {}, neither root nor the provider's user ({user})",
            found.uid
        ));
    }
    if found.mode & OTHERS_ONLY_WRITE != 0 {
        return Err(format!(
            "can be written by others (mode {:o})",
            found.mode & 0o7777
        ));
    }
    if ![Some(0), Some(user)].contains(&found.dir_uid) {
        return Err(format!(
            "its directory belongs to {}, neither root nor the provider's user ({user})",
            found.dir_uid.map_or_else(
                || "no user it can read".to_owned(),
                |uid| format!("user {uid}")
            )
        ));
    }
    if found.dir_mode & OTHERS_WRITE != 0 {
        return Err(format!(
            "its directory can be written by its group or by others (mode {:o})",
            found.dir_mode & 0o7777
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Found, judge};

    /// A socket of the provider's user 7, mode 0660, in a directory of root, mode 0755.
    fn good() -> Found {
        Found {
            socket: true,
            uid: 7,
            mode: 0o140_660,
            dir_uid: Some(0),
            dir_mode: 0o040_755,
        }
    }

    /// Asserts that a socket is taken when it belongs to the provider's user or root, with
    /// its group's write bit, in a directory of root or the provider's user that its group and
    /// others cannot write; and refused, naming the rule, when it is not a socket, belongs to
    /// another user, can be written by others, or lies in a directory of another user, one that
    /// cannot be read, or one its group or others can write. Catches a socket a third user
    /// could have put in the provider's place taken, and a provider that binds its own socket
    /// as its own user refused.
    #[test]
    fn a_socket_is_taken_only_from_the_providers_user_in_a_closed_directory() {
        assert_eq!(
            judge(&good(), 7),
            Ok(()),
            "the provider's socket in root's directory"
        );
        let of_root = Found { uid: 0, ..good() };
        assert_eq!(judge(&of_root, 7), Ok(()), "root's socket");
        let own_dir = Found {
            dir_uid: Some(7),
            ..good()
        };
        assert_eq!(judge(&own_dir, 7), Ok(()), "the provider's directory");
        let refused = [
            (
                Found {
                    socket: false,
                    ..good()
                },
                "not a socket",
            ),
            (
                Found { uid: 3, ..good() },
                "belongs to user 3, neither root nor the provider's user (7)",
            ),
            (
                Found {
                    mode: 0o140_662,
                    ..good()
                },
                "can be written by others (mode 662)",
            ),
            (
                Found {
                    dir_uid: Some(3),
                    ..good()
                },
                "its directory belongs to user 3, neither root nor the provider's user (7)",
            ),
            (
                Found {
                    dir_uid: None,
                    ..good()
                },
                "its directory belongs to no user it can read, neither root nor the provider's \
                 user (7)",
            ),
            (
                Found {
                    dir_mode: 0o040_775,
                    ..good()
                },
                "its directory can be written by its group or by others (mode 775)",
            ),
            (
                Found {
                    dir_mode: 0o041_777,
                    ..good()
                },
                "its directory can be written by its group or by others (mode 1777)",
            ),
        ];
        for (found, why) in refused {
            assert_eq!(judge(&found, 7), Err(why.to_owned()), "{found:?}");
        }
    }
}
