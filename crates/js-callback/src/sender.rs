//! One try of a callback (`docs/design/job-retry-and-completion-callback.md`, sections 5.3 to
//! 5.6): a bare POST with an empty body and the three Standard Webhooks headers, over https,
//! to an address the [`AddressRule`] admitted, and what the answer means
//! ([`CallbackAnswer`]).
//!
//! - **Resolve, check, then connect to the checked address.** The check runs inside ureq's
//!   resolver hook (`ureq::unversioned::resolver::Resolver`, given to the agent with
//!   `Agent::with_parts`). At each try the hook reads an address literal itself, or asks the
//!   [`Resolve`] once; it refuses the try if any address is denied, and hands ureq only the
//!   addresses it checked, which are the only ones ureq connects to. A name that resolves
//!   inward on a later try is refused on that try. The workspace pins ureq to one exact
//!   version, because that module is outside ureq's semver promise.
//! - **No proxy, no redirect, no body read, a time limit.** The agent reads no proxy setting,
//!   follows no redirect (a 3xx is an answer, and a failure), never reads the answer's body,
//!   keeps no idle connection, and bounds a whole try, the lookup included, by its limit.
//! - **The answer classes.** 2xx delivers, 410 abandons, and anything else fails: another
//!   status, a timeout, a refused connection, a redirect, a name that does not resolve, or an
//!   address the rule refused, the last with its own class. The submitter sees only that
//!   class; the detail goes to the operator's log, with the URL's host and never its path or
//!   query. The store (`js-jobs`, `JobStore::answer_callback`) decides when a failed call is
//!   tried again.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use tracing::warn;
use ureq::Agent;
use ureq::config::{AutoHeaderValue, Config};
use ureq::http::Uri;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::address::{AddressRule, literal};
use crate::resolve::Resolve;
use crate::sign::sign;
use crate::{CallbackAnswer, CallbackUrl, FailureClass, SigningKey};

/// The most addresses of one try handed to ureq, which holds no more.
const MAX_ADDRESSES: usize = 16;

/// The port of an https URL that names none.
const HTTPS_PORT: u16 = 443;

/// The certificate authorities a callback's TLS certificate must chain to.
#[derive(Debug, Clone)]
pub struct TlsRoots(RootCerts);

impl TlsRoots {
    /// The WebPki roots bundled in the program, which the server uses, as the alert webhook
    /// does.
    #[must_use]
    pub fn webpki() -> TlsRoots {
        TlsRoots(RootCerts::WebPki)
    }

    /// The certificates `der` (each DER-encoded) and no others: a test's own authority.
    #[must_use]
    pub fn from_der(der: &[Vec<u8>]) -> TlsRoots {
        let certificates = der.iter().map(|one| Certificate::from_der(one).to_owned());
        TlsRoots(RootCerts::Specific(Arc::new(certificates.collect())))
    }
}

/// What tries a callback: an HTTP agent that takes no proxy, follows no redirect, reads no
/// answer's body and connects only to addresses its rule admitted.
#[derive(Debug)]
pub struct Sender {
    /// The agent, with the checking resolver.
    agent: Agent,
}

impl Sender {
    /// A sender whose tries each take at most `limit`, that connects only where `rule` admits,
    /// asks `resolver` for a name's addresses, and trusts certificates that chain to `roots`.
    /// The server passes 10 s, its rule, [`crate::SystemResolver`] and
    /// [`TlsRoots::webpki`]; a test passes a rule that admits loopback, a fake resolver and
    /// its own authority.
    #[must_use]
    pub fn new(
        limit: Duration,
        rule: AddressRule,
        resolver: Box<dyn Resolve + Send + Sync>,
        roots: TlsRoots,
    ) -> Sender {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(roots.0)
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let config = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(true)
            .timeout_global(Some(limit))
            .max_idle_connections(0)
            .user_agent(AutoHeaderValue::None)
            .accept(AutoHeaderValue::None)
            .accept_encoding(AutoHeaderValue::None)
            .tls_config(tls)
            .build();
        let checked = Checked { rule, resolver };
        Sender {
            agent: Agent::with_parts(config, DefaultConnector::default(), checked),
        }
    }

    /// Tries the callback once: POSTs to `url` an empty body with the headers `webhook-id`
    /// (`webhook_id`), `webhook-timestamp` (`timestamp`, seconds since the epoch) and
    /// `webhook-signature` ([`sign`] under `key`), and says what the answer means.
    pub fn send(
        &self,
        url: &CallbackUrl,
        key: &SigningKey,
        webhook_id: &str,
        timestamp: u64,
    ) -> CallbackAnswer {
        let sent = self
            .agent
            .post(url.as_str())
            .header("webhook-id", webhook_id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", sign(key, webhook_id, timestamp))
            .send_empty();
        match sent {
            Ok(answer) => answered(url, answer.status().as_u16()),
            Err(error) => failed(url, &error),
        }
    }
}

/// What an answer with `status` means. The body is never read: the answer is dropped with its
/// connection.
fn answered(url: &CallbackUrl, status: u16) -> CallbackAnswer {
    match status {
        200..=299 => CallbackAnswer::Delivered,
        410 => CallbackAnswer::Abandoned,
        _ => {
            warn!(
                host = url.host(),
                status, "a callback was answered with neither 2xx nor 410"
            );
            CallbackAnswer::Failed(FailureClass::Failed)
        }
    }
}

/// What a try that got no answer means, logged for the operator. ureq's error text never holds
/// the URL's path or query for a URL [`crate::check_url`] passed: only a URL with no scheme or
/// host, or one that is not https, would be repeated, and none reaches here.
fn failed(url: &CallbackUrl, error: &ureq::Error) -> CallbackAnswer {
    warn!(host = url.host(), %error, "a callback try failed");
    let refused = matches!(
        error,
        ureq::Error::Io(cause) if cause.get_ref().is_some_and(|inner| inner.is::<Refused>())
    );
    let class = if refused {
        FailureClass::RefusedAddress
    } else {
        FailureClass::Failed
    };
    CallbackAnswer::Failed(class)
}

/// The address rule refused an address the host resolved to, so the try connected nowhere.
#[derive(Debug, thiserror::Error)]
#[error("the address rule refuses {0}")]
struct Refused(std::net::IpAddr);

/// ureq's resolver hook: the addresses of the URL's host, each of which the rule admits.
#[derive(Debug)]
struct Checked {
    /// What may be connected to.
    rule: AddressRule,
    /// What names a host's addresses.
    resolver: Box<dyn Resolve + Send + Sync>,
}

impl Resolver for Checked {
    fn resolve(
        &self,
        uri: &Uri,
        _: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().unwrap_or_default();
        let found = match literal(host) {
            Some(address) => vec![address],
            None => self
                .resolver
                .resolve(host, *timeout.after)
                .map_err(|error| ureq::Error::Io(io::Error::other(error)))?,
        };
        if let Some(denied) = found.iter().find(|address| !self.rule.admits(**address)) {
            let refused = Refused(*denied);
            return Err(ureq::Error::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                refused,
            )));
        }
        let port = uri.port_u16().unwrap_or(HTTPS_PORT);
        let mut checked = self.empty();
        for address in found.into_iter().take(MAX_ADDRESSES) {
            checked.push(SocketAddr::new(address, port));
        }
        if checked.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(checked)
    }
}

#[cfg(test)]
mod receiver;
#[cfg(test)]
mod tests;
