//! What a connection to the lease server proves who it is with: a bearer read from a file,
//! or the host's own certificate and key, which hostkeeper keeps and renews in place.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_client::{Client, ClientError, HostIdentity};

use crate::{Cli, CredentialFile, Failure};

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

/// How the transport first proves who it is: the host's certificate and key when
/// `--host-cert` and `--host-key` are given (then no `--token-file` is needed, the token file
/// and the host credential file are not read, and no bearer is sent), and otherwise
/// [`bearer_file`].
///
/// # Errors
///
/// Whatever [`bearer_file`] refuses, when no host certificate is given.
pub(crate) fn login(cli: &Cli) -> Result<Login, Failure> {
    match Login::host_certificate(cli.host_cert.as_deref(), cli.host_key.as_deref()) {
        Some(login) => Ok(login),
        None => bearer_file(cli).map(Login::Token),
    }
}

/// The file the transport first reads its bearer from: `--token-file` when given, and
/// otherwise the host credential file of `--state-root`, which has the same form (one run
/// of printable ASCII, owner-only). The agent adopts the credential from the same file
/// again below, so a file that is there and not a credential stops it either way.
///
/// # Errors
///
/// [`Failure::Usage`] naming `--token-file` when there is neither, and the path of the
/// credential file when it is there and cannot be used.
fn bearer_file(cli: &Cli) -> Result<PathBuf, Failure> {
    if let Some(token_file) = &cli.token_file {
        return Ok(token_file.clone());
    }
    let kept = match &cli.state_root {
        Some(root) => {
            let file = CredentialFile::in_dir(root);
            file.load().map_err(Failure::Usage)?.map(|_| file.path())
        }
        None => None,
    };
    kept.ok_or_else(|| {
        Failure::Usage(
            "--token-file: required unless --state-root holds a host credential".to_owned(),
        )
    })
}
