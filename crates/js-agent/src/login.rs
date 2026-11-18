//! What a connection to the lease server proves who it is with: a bearer read from a file,
//! or the host's own certificate and key, which hostkeeper keeps and renews in place.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_client::{Client, ClientError, HostIdentity};

/// How the agent, the cordon verb and the update hook authenticate to the lease server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// A bearer in an owner-only file: the host credential of the state root or the join
    /// token (`--token-file`).
    Token(PathBuf),
    /// The host's certificate and key (`--host-cert` and `--host-key`): the certificate is
    /// presented in the TLS handshake, with no bearer, and both files are read again before
    /// each request.
    HostCertificate(HostIdentity),
}

impl Login {
    /// The host-certificate login the flags `--host-cert` and `--host-key` name: `Some` when
    /// both are given, `None` when neither is. The flags are given both or neither (the
    /// command lines require it), so a lone one is treated as none here.
    #[must_use]
    pub fn host_certificate(cert: Option<&Path>, key: Option<&Path>) -> Option<Self> {
        cert.zip(key)
            .map(|(cert, key)| Self::HostCertificate(HostIdentity::new(cert, key)))
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
            Self::HostCertificate(identity) => {
                Client::connect_with_identity(server, identity, ca_cert, limit)
            }
        }
    }
}
