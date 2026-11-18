//! What a connection to the lease server proves who it is with: a bearer read from a file,
//! or the service identity hostkeeper keeps in the agent's state root.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_client::{Client, ClientError, ServiceIdentity};

/// How the agent, the cordon verb and the update hook authenticate to the lease server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// A bearer in an owner-only file: the host credential of the state root or the join
    /// token (`--token-file`).
    Token(PathBuf),
    /// The service identity of the state root: a client certificate presented in the TLS
    /// handshake, with no bearer, read again before each request.
    Identity(ServiceIdentity),
}

impl Login {
    /// The identity of the state root `root` when it holds one: [`Login::Identity`] for a
    /// root with both identity files, `None` for one with neither.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming `--state-root` and the missing file when the root holds
    /// one of the two files only.
    pub fn identity_of(root: &Path) -> Result<Option<Self>, ClientError> {
        ServiceIdentity::found_in(root).map(|found| found.map(Self::Identity))
    }

    /// Connects as this login says, with every request bounded by `limit`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming the flag that is wrong (see [`Client::connect`] and
    /// [`Client::connect_with_identity`]).
    pub fn connect(
        &self,
        server: &str,
        ca_cert: Option<&Path>,
        limit: Duration,
    ) -> Result<Client, ClientError> {
        match self {
            Self::Token(file) => Client::connect(server, file, ca_cert, limit),
            Self::Identity(identity) => {
                Client::connect_with_identity(server, identity, ca_cert, limit)
            }
        }
    }
}
