//! The URL rule and the HTTP agent of the callers that dial out: the webhook sink and the
//! watcher's status read.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Whether plain `http://` may name any address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plain {
    /// Only a loopback address literal: what is sent may hold a secret.
    LoopbackOnly,
    /// Any address: what is sent holds no secret.
    Any,
}

/// Why a URL was refused. No variant repeats the URL: it may hold a secret path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    /// The URL does not start with `https://` or `http://`.
    #[error("the URL must start with https:// or http://")]
    Scheme,
    /// The URL has no host, a space or control character, a user, or a fragment.
    #[error("the URL needs a host and no space, control character, user or fragment")]
    Shape,
    /// A plain `http://` URL names an address that is not on the loopback interface.
    #[error("plain http is accepted for a loopback address only")]
    PlainHttp,
}

/// Checks `url` and says whether it is `https`.
///
/// # Errors
///
/// [`UrlError`] naming the rule broken.
pub fn check(url: &str, plain: Plain) -> Result<bool, UrlError> {
    let (https, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        (None, None) => return Err(UrlError::Scheme),
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = rest.split_at(end).0;
    let odd = |c: char| c.is_ascii_control() || c == ' ' || c == '#';
    if authority.is_empty() || authority.contains('@') || url.chars().any(odd) {
        return Err(UrlError::Shape);
    }
    if !https && plain == Plain::LoopbackOnly && !loopback(authority) {
        return Err(UrlError::PlainHttp);
    }
    Ok(https)
}

/// Whether `authority` is an IP address literal on the loopback interface.
fn loopback(authority: &str) -> bool {
    authority
        .parse::<SocketAddr>()
        .map(|addr| addr.ip())
        .or_else(|_| authority.parse::<IpAddr>())
        .is_ok_and(|ip| ip.is_loopback())
}

/// A client that follows no redirect, reads no proxy setting, holds no idle connection,
/// treats an HTTP error status as an answer, and bounds a whole call by `limit`. `https` is
/// what [`check`] said of the URL it will dial; the roots trusted are the public ones
/// bundled in the program.
pub fn agent(https: bool, limit: Duration) -> Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(RootCerts::WebPki)
        .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
        .build();
    Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(https)
        .timeout_global(Some(limit))
        .max_idle_connections(0)
        .tls_config(tls)
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts each rule of the URL check, and that a refusal never repeats the URL. It
    /// catches a webhook that would send a bearer token over plain http to a remote host,
    /// and a URL with a user or a fragment sneaking past.
    #[test]
    fn the_url_rule_refuses_what_could_leak_and_accepts_the_rest() {
        let strict = |url| check(url, Plain::LoopbackOnly);
        assert_eq!(strict("https://hooks.example.org/x/SECRET?a=1"), Ok(true));
        assert_eq!(strict("http://127.0.0.1:8080/path"), Ok(false));
        assert_eq!(strict("http://[::1]:8080"), Ok(false));
        assert_eq!(strict("http://127.0.0.1"), Ok(false));
        assert_eq!(strict("ftp://127.0.0.1"), Err(UrlError::Scheme));
        assert_eq!(strict("https://"), Err(UrlError::Shape));
        assert_eq!(strict("https:///path"), Err(UrlError::Shape));
        assert_eq!(strict("https://u@host/"), Err(UrlError::Shape));
        assert_eq!(strict("https://host/a b"), Err(UrlError::Shape));
        assert_eq!(strict("https://host/a#frag"), Err(UrlError::Shape));
        assert_eq!(strict("http://10.0.0.5:7300"), Err(UrlError::PlainHttp));
        assert_eq!(strict("http://localhost:7300"), Err(UrlError::PlainHttp));
        assert_eq!(check("http://10.0.0.5:7300", Plain::Any), Ok(false));
        let shown = strict("http://SECRET-host").unwrap_err().to_string();
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    /// Asserts that an agent can be built for both schemes. It catches a TLS setup that
    /// panics at construction (a missing crypto provider), which no loopback test reaches.
    #[test]
    fn an_agent_builds_for_http_and_https() {
        let _plain = agent(false, Duration::from_secs(1));
        let _tls = agent(true, Duration::from_secs(1));
    }
}
