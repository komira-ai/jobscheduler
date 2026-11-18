//! What the agent routes do with a client certificate and with the deny list.
//!
//! A connection whose client presented a service certificate ([`crate::service_cert`])
//! carries the host the certificate names. On the agent routes that certificate is the
//! credential: a bearer on the same request is not read, the credential the registry binds
//! is derived from the host id alone ([`Credential::of_service_certificate`]), so a renewed
//! certificate is the same credential, and the host a request is about (the body's of a hello
//! or a heartbeat, the path's of an own-host route) must be the certificate's. No one-time
//! token is consumed, no host credential is issued, and the hello's own `pool` is taken as it
//! says. Every agent route also refuses a host on the deny list ([`crate::deny`]), whatever
//! the host presented.

use axum::extract::Request;
use axum::http::StatusCode;
use js_agent_wire::{Hello, Welcome};
use js_registry::{Credential, Refusal};
use js_types::HostId;
use tracing::warn;

use super::Refused;
use crate::agents::Agents;
use crate::deny::HOST_DENIED;
use crate::service_cert::CertHost;

/// The code of the 403 a request about another host than its certificate's is answered with.
pub(crate) const CERTIFICATE_HOST_MISMATCH: &str = "certificate-host-mismatch";

/// Marks a request authenticated by the service certificate of its connection: the host the
/// certificate names.
#[derive(Clone)]
pub(super) struct ServiceHost(pub(super) HostId);

/// When the connection of `request` carries the host of a client certificate, makes the
/// certificate the request's credential and marks it [`ServiceHost`]; true then. A request
/// without one is left as it is.
pub(super) fn admit(request: &mut Request) -> bool {
    let Some(CertHost(host)) = request.extensions().get::<CertHost>().cloned() else {
        return false;
    };
    let credential = Credential::of_service_certificate(&host);
    request.extensions_mut().insert(credential);
    request.extensions_mut().insert(ServiceHost(host));
    true
}

/// Refuses a request about the host named `host` (403) when its certificate names another
/// (`service`, when the request carries one), and when the host is on the deny list.
pub(super) fn check(
    agents: &Agents,
    service: Option<&ServiceHost>,
    host: &str,
) -> Result<(), Refused> {
    if service.is_some_and(|service| service.0.as_str() != host) {
        warn!(%host, "agent request refused: its certificate names another host");
        return Err(Refused::new(
            StatusCode::FORBIDDEN,
            CERTIFICATE_HOST_MISMATCH,
        ));
    }
    if agents.deny.denies_text(host) {
        warn!(%host, "agent request refused: the host is denied");
        return Err(Refused::new(StatusCode::FORBIDDEN, HOST_DENIED));
    }
    Ok(())
}

/// The registry's answer to `hello`: a hello with a service certificate (`service`) may move
/// a host that is not live from another credential onto the certificate's
/// ([`js_registry::Registry::hello_as_service`]).
pub(super) fn admit_hello(
    agents: &Agents,
    service: bool,
    hello: &Hello,
    who: &Credential,
) -> Result<Welcome, Refusal> {
    match service {
        true => agents.registry.hello_as_service(hello, who),
        false => agents.registry.hello(hello, who),
    }
}
