//! The real client: one blocking request per call, under a time limit.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Error as TransportError};

use crate::api::LambdaApi;
use crate::error::{ApiError, cut};
use crate::key::ApiKey;
use crate::model::{Instance, InstanceId, InstanceTypeOffer, LaunchRequest};
use crate::wire;

/// The path every call lives under.
const API: &str = "/api/v1";

/// The largest answer read.
const ANSWER_LIMIT: u64 = 8 * 1024 * 1024;

/// The longest transport error text kept.
const QUOTED: usize = 200;

/// Why a client could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    /// The base URL is not `http://host:port` or `https://host[:port]` without a path,
    /// user, query or fragment.
    #[error("the base URL must be scheme://host[:port] with no path, user, query or fragment")]
    BaseUrl,
    /// A plain `http://` base URL names an address that is not on the loopback interface;
    /// the key would cross the network unencrypted.
    #[error("plain http is accepted for a loopback address only, not {0}")]
    PlainHttpNotLoopback(String),
    /// An `https://` base URL needs the PEM file of the roots to trust: the platform's
    /// roots are found through the environment and are not read.
    #[error("an https base URL needs a CA bundle")]
    CaBundleRequired,
    /// The CA bundle cannot be used.
    #[error("{path}: {why}")]
    CaBundle {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
}

/// The client for one account, bound to one base URL and one key. The agent reads no
/// proxy setting, follows no redirect and holds no idle connection, so the key goes to the
/// one host named and nowhere else.
pub struct HttpApi {
    agent: Agent,
    base: String,
    authorization: String,
}

impl fmt::Debug for HttpApi {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("HttpApi")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl HttpApi {
    /// Builds the client. There is no default address: `base_url` is the caller's, as
    /// `https://host` (with `ca_bundle`, the PEM file of the roots to trust) or, for a
    /// local test server, `http://127.0.0.1:port`. Every request is answered or fails
    /// within `limit`, connect and TLS included.
    ///
    /// # Errors
    ///
    /// [`SetupError`] when the URL or the bundle is unusable.
    pub fn connect(
        base_url: &str,
        key: &ApiKey,
        ca_bundle: Option<&Path>,
        limit: Duration,
    ) -> Result<Self, SetupError> {
        let (https, base) = parse_base(base_url)?;
        let roots = match (https, ca_bundle) {
            (false, _) => Vec::new(),
            (true, Some(path)) => pem_roots(path)?,
            (true, None) => return Err(SetupError::CaBundleRequired),
        };
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::Specific(Arc::new(roots)))
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let agent: Agent = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            // Redirects are off, so this cannot differ from them today; it keeps an
            // https client from ever speaking plain http should that change.
            .https_only(https)
            .timeout_global(Some(limit))
            .max_idle_connections(0)
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            agent,
            base,
            authorization: key.authorization(),
        })
    }

    /// `GET path`: the body of a success, or the failure.
    fn get(&self, path: &str) -> Result<Vec<u8>, ApiError> {
        self.send(path, None)
    }

    /// `POST path` with a JSON `body`: the body of a success, or the failure.
    fn post(&self, path: &str, body: &[u8]) -> Result<Vec<u8>, ApiError> {
        self.send(path, Some(body))
    }

    fn send(&self, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>, ApiError> {
        let transport = |err: TransportError| ApiError::Transport(cut(&err.to_string(), QUOTED));
        let url = format!("{}{API}{path}", self.base);
        let sent = match body {
            None => self
                .agent
                .get(url)
                .header("authorization", &self.authorization)
                .header("accept", "application/json")
                .call(),
            Some(bytes) => self
                .agent
                .post(url)
                .header("authorization", &self.authorization)
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .send(bytes),
        };
        let mut response = sent.map_err(transport)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let bytes = response
            .body_mut()
            .with_config()
            .limit(ANSWER_LIMIT)
            .read_to_vec()
            .map_err(transport)?;
        tracing::debug!(path, status, bytes = bytes.len(), "lambda answered");
        if (200..300).contains(&status) {
            return Ok(bytes);
        }
        let (code, request_id) = wire::error_parts(&bytes);
        tracing::debug!(path, status, %code, ?request_id, "lambda refused");
        Err(match status {
            429 => ApiError::RateLimited {
                retry_after,
                request_id,
            },
            _ => ApiError::Api {
                status,
                code,
                request_id,
            },
        })
    }
}

impl LambdaApi for HttpApi {
    fn instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError> {
        wire::offers(&self.get("/instance-types")?)
    }

    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        wire::instances(&self.get("/instances")?)
    }

    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError> {
        let found = wire::one_instance(&self.get(&format!("/instances/{id}"))?)?;
        // A caller that terminates what it read must be sure it read the one it asked for.
        match found.id == *id {
            true => Ok(found),
            false => Err(ApiError::Malformed(
                "the answer is for another instance than the one asked for".to_owned(),
            )),
        }
    }

    fn launch(&self, request: &LaunchRequest) -> Result<Vec<InstanceId>, ApiError> {
        wire::launched(&self.post("/instance-operations/launch", &wire::launch_body(request))?)
    }

    fn terminate(&self, id: &InstanceId) -> Result<Vec<InstanceId>, ApiError> {
        wire::terminated(&self.post("/instance-operations/terminate", &wire::terminate_body(id))?)
    }
}

/// Whether the URL is https, and the base to build request URLs on (no trailing slash).
fn parse_base(url: &str) -> Result<(bool, String), SetupError> {
    let (https, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        (None, None) => return Err(SetupError::BaseUrl),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let odd = |c: char| c.is_ascii_control() || " /?#@\\".contains(c);
    if authority.is_empty() || authority.chars().any(odd) {
        return Err(SetupError::BaseUrl);
    }
    if !https && !loopback(authority) {
        return Err(SetupError::PlainHttpNotLoopback(authority.to_owned()));
    }
    let scheme = if https { "https" } else { "http" };
    Ok((https, format!("{scheme}://{authority}")))
}

/// Whether `authority` is an IP address literal on the loopback interface.
fn loopback(authority: &str) -> bool {
    authority
        .parse::<SocketAddr>()
        .map(|addr| addr.ip())
        .or_else(|_| authority.parse::<IpAddr>())
        .is_ok_and(|ip| ip.is_loopback())
}

/// The certificates in a PEM file; a file without one is refused.
fn pem_roots(path: &Path) -> Result<Vec<Certificate<'static>>, SetupError> {
    let refuse = |why: String| SetupError::CaBundle {
        path: path.to_owned(),
        why,
    };
    let found = CertificateDer::pem_file_iter(path)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| refuse(err.to_string()))?;
    match found.is_empty() {
        true => Err(refuse("no certificate found".to_owned())),
        false => Ok(found
            .iter()
            .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
            .collect()),
    }
}
