//! What a connection to the lease server proves who it is with: the host's own certificate
//! and key, which hostkeeper keeps and renews in place. A host has no other identity.

use std::path::Path;
use std::time::Duration;

use js_client::{Client, ClientError, HostIdentity};

use crate::{Cli, Failure};

/// How the agent, the cordon verb and the update hook authenticate to the lease server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
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
    /// [`ClientError::Flag`] naming the flag that is wrong (see
    /// [`Client::connect_with_identity`]).
    pub fn connect(
        &self,
        server: &str,
        ca_cert: Option<&Path>,
        limit: Duration,
    ) -> Result<Client, ClientError> {
        match self {
            Self::HostCertificate(identity) => {
                Client::connect_with_identity(server, identity, ca_cert, limit)
            }
        }
    }
}

/// How the transport proves who it is: the host's certificate and key the flags
/// `--host-cert` and `--host-key` name.
///
/// # Errors
///
/// [`Failure::Usage`] naming both flags when neither is given: a host is identified by its
/// certificate alone, so an agent without one does not start.
pub(crate) fn login(cli: &Cli) -> Result<Login, Failure> {
    Login::host_certificate(cli.host_cert.as_deref(), cli.host_key.as_deref()).ok_or_else(|| {
        Failure::Usage(
            "--host-cert and --host-key: required (the host's certificate and key, which \
             hostkeeper keeps); a host is identified by its certificate alone"
                .to_owned(),
        )
    })
}
