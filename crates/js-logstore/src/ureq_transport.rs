//! The production transport: one blocking request per call, under a time limit.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::http::{HttpRequest, HttpResponse, Transport, TransportError};

/// The most bytes of an answer's body the transport reads: a list page or a ranged read of a
/// chunk is well under it. A longer body is [`TransportError::TooLarge`].
pub const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

/// The transport over `ureq`. It reads no proxy setting, follows no redirect, holds no idle
/// connection, and bounds each exchange, connect and TLS included, by the limit it is built with.
/// For `https` it trusts the public roots bundled in the `webpki-roots` crate (pinned by
/// `Cargo.lock`); the platform's store is never read, since its loaders find it through
/// environment variables.
pub struct UreqTransport {
    agent: Agent,
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("UreqTransport").finish_non_exhaustive()
    }
}

impl UreqTransport {
    /// A transport whose every exchange is answered or fails within `limit`.
    pub fn new(limit: Duration) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let agent: Agent = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(limit))
            .max_idle_connections(0)
            .tls_config(tls)
            .build()
            .into();
        Self { agent }
    }
}

impl Transport for UreqTransport {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let built = builder
            .body(request.body)
            .map_err(|_| TransportError::Request)?;
        let mut response = self.agent.run(built).map_err(failure)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                let value = value.to_str().ok()?;
                Some((name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            // ureq's limit refuses a body that reaches it (it reads once more to see the
            // end), so one byte more lets a body of exactly the cap through.
            .limit(MAX_RESPONSE_BYTES + 1)
            .read_to_vec()
            .map_err(failure)?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// The kind of a `ureq` failure. Its text is not kept: it may hold the URL.
fn failure(error: ureq::Error) -> TransportError {
    match error {
        ureq::Error::Timeout(_) => TransportError::Timeout,
        ureq::Error::Io(error) => TransportError::Io(error.kind()),
        ureq::Error::BodyExceedsLimit(_) => TransportError::TooLarge,
        _ => TransportError::Exchange,
    }
}
