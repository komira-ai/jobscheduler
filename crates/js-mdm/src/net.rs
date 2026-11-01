//! Downloads, over HTTPS only, for `fetch`.

use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// How long one download may take, whole.
const TIMEOUT: Duration = Duration::from_secs(600);

/// Fetches a URL's body.
pub trait Download {
    /// The body of `url`, which must answer 2xx and hold at most `limit` bytes.
    ///
    /// # Errors
    /// Why the download failed, as text.
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String>;
}

/// [`Download`] over ureq: redirects are followed (release assets are served from a
/// redirect target), every hop must be `https://` when `https_only`, no proxy setting is
/// read, and the public web roots are trusted.
pub struct Net {
    agent: Agent,
}

impl Net {
    /// An agent; `https_only` is true outside tests.
    pub fn new(https_only: bool) -> Net {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let agent = Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .https_only(https_only)
            .timeout_global(Some(TIMEOUT))
            .tls_config(tls)
            .build()
            .into();
        Net { agent }
    }
}

impl Download for Net {
    fn get(&self, url: &str, _limit: u64) -> Result<Vec<u8>, String> {
        let _ = &self.agent;
        Err(format!("not written yet ({url})"))
    }
}
