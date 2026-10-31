//! The lease server's routes for a host's own cordon, as a port, and the client that calls
//! them.
//!
//! A host cordons itself with `POST /v1/hosts/{id}:cordon`, lifts its own cordon with
//! `POST /v1/hosts/{id}:uncordon` and reads itself with `GET /v1/hosts/{id}`, all with the
//! credential it was admitted with; the shared pool token is refused. [`Lease`] is the port
//! (the update hook's steps and `js-agent cordon` take it), [`HttpLease`] is the client over
//! `js-client`. Nothing here prints or keeps the credential: it is read from its file by
//! `js-client`, held there, and sent in the `Authorization` header only.

use std::path::Path;

use js_agent_wire::Taint;
use js_client::{Client, ClientError};
use serde::{Deserialize, Serialize};

use crate::transport::REQUEST_LIMIT;

/// The longest cordon the lease server grants in one request; a longer ask is cut to it.
pub const MAX_CORDON_SECS: u64 = 3600;

/// When a cordon lapses, on the lease server's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CordonView {
    /// Milliseconds since the epoch.
    pub expires_at_ms: u64,
    /// Milliseconds from the server's now to `expires_at_ms`, when the server said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

/// What `GET /v1/hosts/{id}` says of a host's own host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReading {
    /// The host id.
    pub id: String,
    /// `awaiting`, `live` or `lost`.
    pub state: String,
    /// One more at each hello of the host; 0 before the first since the server started.
    pub epoch: u64,
    /// When the current session began, on the server's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_started_ms: Option<u64>,
    /// The leases the host holds: 0 or 1.
    pub leases: u64,
    /// The lease, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// The host holds no lease but has runs to finish.
    pub releasing: bool,
    /// How the host is set aside, as `GET /v1/hosts` lists it: `reserved`, `held`,
    /// `draining` or `releasing`. Absent for a host that is not set aside, and from a server
    /// that does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<String>,
    /// The cordon in force, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cordon: Option<CordonView>,
    /// The taint the host reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<Taint>,
    /// The `agent_version` of the last hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// The `runtime` of the last hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

/// The reply to a cordon.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Cordoned {
    /// The host was cordoned before this call.
    pub already_cordoned: bool,
    /// The host after the call.
    #[serde(flatten)]
    pub host: HostReading,
}

/// The reply to an uncordon.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Uncordoned {
    /// A cordon was in force and this call ended it.
    pub lifted: bool,
    /// The host after the call.
    #[serde(flatten)]
    pub host: HostReading,
}

/// Why the lease server did not answer as asked.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LeaseError {
    /// The server holds no host by this id (`unknown-host`): it has not said hello since the
    /// server started.
    #[error("unknown host")]
    UnknownHost,
    /// The server refused the request (`invalid-cordon`, `join-token-not-admitted`, ...), by
    /// its code, or by `http-<status>` when it sent none.
    #[error("refused: {0}")]
    Refused(String),
    /// The server could not be reached, or answered what is not understood.
    #[error("{0}")]
    Unreachable(String),
}

/// The lease server's routes for the caller's own host.
pub trait Lease {
    /// `POST /v1/hosts/{id}:cordon {"lease_secs"}`: cordons for `lease_secs` from now (cut to
    /// [`MAX_CORDON_SECS`]). A repeat extends the cordon and never shortens it.
    ///
    /// # Errors
    /// [`LeaseError`].
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError>;

    /// `POST /v1/hosts/{id}:uncordon`. Succeeds when there was no cordon.
    ///
    /// # Errors
    /// [`LeaseError`].
    fn uncordon(&self) -> Result<Uncordoned, LeaseError>;

    /// `GET /v1/hosts/{id}`.
    ///
    /// # Errors
    /// [`LeaseError`].
    fn host(&self) -> Result<HostReading, LeaseError>;
}

/// [`Lease`] over HTTP/1.1, through the pinned-root client of `js-client`, for one host.
///
/// It has no `Debug`: it holds the credential.
pub struct HttpLease {
    client: Client,
    host: String,
}

impl HttpLease {
    /// Connects to `server` as `host`, with the credential in the owner-only file
    /// `credential`, trusting `ca_cert` for `https://` (or the bundled public roots without
    /// it). `host` is a valid host id: it has no colon and nothing to escape.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming `--server`, `--token-file` or `--ca-cert` when that
    /// flag, or the file it names, is unusable.
    pub fn connect(
        server: &str,
        credential: &Path,
        ca_cert: Option<&Path>,
        host: &str,
    ) -> Result<Self, ClientError> {
        Client::connect(server, credential, ca_cert, REQUEST_LIMIT).map(|client| Self {
            client,
            host: host.to_owned(),
        })
    }

    /// The path of the route: `verb` is `cordon` or `uncordon`, or none for the read.
    fn path(&self, verb: Option<&str>) -> String {
        verb.map_or_else(
            || format!("/v1/hosts/{}", self.host),
            |verb| format!("/v1/hosts/{}:{verb}", self.host),
        )
    }
}

impl Lease for HttpLease {
    fn cordon(&self, _lease_secs: u64) -> Result<Cordoned, LeaseError> {
        let _ = &self.client;
        let path = self.path(Some("cordon"));
        Err(LeaseError::Unreachable(format!("not implemented: {path}")))
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        let path = self.path(Some("uncordon"));
        Err(LeaseError::Unreachable(format!("not implemented: {path}")))
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        let path = self.path(None);
        Err(LeaseError::Unreachable(format!("not implemented: {path}")))
    }
}
