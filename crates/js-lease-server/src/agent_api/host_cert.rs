//! What the agent routes do with a client certificate.
//!
//! A connection whose client presented a host certificate ([`crate::host_cert`])
//! carries the host the certificate names. On the agent routes that certificate is the
//! credential: a bearer on the same request is not read, the credential the registry binds
//! is derived from the host id alone ([`Credential::of_host_certificate`]), so a renewed
//! certificate is the same credential, and the host a request is about (the body's of a hello
//! or a heartbeat, the path's of an own-host route) must be the certificate's. No one-time
//! token is consumed, no host credential is issued, and the hello's own `pool` is taken as it
//! says.

use axum::extract::Request;
use axum::http::StatusCode;
use js_registry::Credential;
use js_types::HostId;
use tracing::warn;

use super::Refused;
use crate::host_cert::CertHost;

/// The code of the 403 a request about another host than its certificate's is answered with.
pub(crate) const CERTIFICATE_HOST_MISMATCH: &str = "certificate-host-mismatch";

/// Marks a request authenticated by the host certificate of its connection: the host the
/// certificate names.
#[derive(Clone)]
pub(super) struct CertifiedHost(pub(super) HostId);

/// When the connection of `request` carries the host of a client certificate, makes the
/// certificate the request's credential and marks it [`CertifiedHost`]; true then. A request
/// without one is left as it is.
pub(super) fn admit(request: &mut Request) -> bool {
    let Some(CertHost(host)) = request.extensions().get::<CertHost>().cloned() else {
        return false;
    };
    let credential = Credential::of_host_certificate(&host);
    request.extensions_mut().insert(credential);
    request.extensions_mut().insert(CertifiedHost(host));
    true
}

/// Refuses a request about the host named `host` (403) when its certificate names another
/// (`certified`, when the request carries one).
pub(super) fn check(certified: Option<&CertifiedHost>, host: &str) -> Result<(), Refused> {
    if certified.is_some_and(|certified| certified.0.as_str() != host) {
        warn!(%host, "agent request refused: its certificate names another host");
        return Err(Refused::new(
            StatusCode::FORBIDDEN,
            CERTIFICATE_HOST_MISMATCH,
        ));
    }
    Ok(())
}
