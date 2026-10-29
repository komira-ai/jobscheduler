//! The Tailscale API calls the service makes: the OAuth access token, an auth key per
//! launch, and the delete of a device.
//!
//! The client follows no redirect, reads no proxy setting and holds no idle connection,
//! so the credentials go to the one host named. No error holds the body of an answer.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hyper::Uri;
use js_elastic::Secret;
use js_lease::{Clock, Timestamp};
use rustls::crypto::ring::default_provider;
use serde::Deserialize;
use serde_json::json;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Error as TransportError};

use super::{ApiFailure, NetworkError, OAuthClient};
use crate::adapt::SharedClock;

/// The address of Tailscale's API.
pub const API_BASE: &str = "https://api.tailscale.com";

/// The most time one request may take, connect and TLS included.
pub const API_TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes read from one answer. A longer answer is a transport failure.
pub const ANSWER_LIMIT: u64 = 8 * 1024 * 1024;

/// How long before an access token's expiry it is fetched again.
pub const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// How many times a device delete is tried when it fails in a way that may pass.
pub const FORGET_ATTEMPTS: usize = 3;

/// The longest transport error text kept.
const QUOTED: usize = 200;

/// The path every call lives under.
const API: &str = "/api/v2";

/// The access token and when it stops working.
struct Cached {
    token: Secret,
    expires: Timestamp,
}

/// The token endpoint's answer. It has no `Debug`.
#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: u64,
}

/// The key endpoint's answer. It has no `Debug`.
#[derive(Deserialize)]
struct KeyAnswer {
    key: String,
}

/// The device list.
#[derive(Deserialize)]
struct Devices {
    devices: Vec<Device>,
}

/// One device of the list.
#[derive(Deserialize)]
struct Device {
    id: String,
    #[serde(default)]
    hostname: String,
}

/// The client of one tailnet, bound to one base URL and one OAuth client. The access
/// token is kept until [`TOKEN_REFRESH_MARGIN`] before it expires, by the injected clock.
pub struct TailscaleApi {
    agent: Agent,
    base: String,
    tailnet: String,
    client: OAuthClient,
    clock: SharedClock,
    cached: Mutex<Option<Cached>>,
}

impl fmt::Debug for TailscaleApi {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("TailscaleApi")
            .field("base", &self.base)
            .field("tailnet", &self.tailnet)
            .finish_non_exhaustive()
    }
}

/// Whether `text` can stand as one path segment: letters, digits, `.`, `_` and `-`.
fn path_segment(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `host` is an address on the loopback interface.
fn loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn cut(text: &str) -> String {
    text.chars().take(QUOTED).collect()
}

impl TailscaleApi {
    /// The client for `tailnet` (`-` is the credential's own), at `base_url`: `https://host`,
    /// or `http://` with a loopback host for a test server. Every request is answered or
    /// fails within `timeout`.
    ///
    /// # Errors
    ///
    /// [`NetworkError::Misconfigured`] when the base URL is not of that form or the
    /// tailnet cannot stand in a path.
    pub fn new(
        base_url: &str,
        tailnet: &str,
        client: OAuthClient,
        clock: SharedClock,
        timeout: Duration,
    ) -> Result<Self, NetworkError> {
        let bad_url = || {
            NetworkError::Misconfigured(format!(
                "{base_url:?} must be https://host[:port], or http:// with a loopback host, with no path"
            ))
        };
        let uri: Uri = base_url.parse().map_err(|_| bad_url())?;
        let host = uri.host().unwrap_or_default();
        // Nothing follows the authority: no path, query or trailing slash.
        let no_path = uri
            .authority()
            .is_some_and(|authority| base_url.ends_with(authority.as_str()));
        let https = match uri.scheme_str() {
            Some("https") if no_path => true,
            Some("http") if no_path && loopback(host) => false,
            _ => return Err(bad_url()),
        };
        if !path_segment(tailnet) {
            return Err(NetworkError::Misconfigured(format!(
                "tailnet {tailnet:?} must be letters, digits, '.', '_' or '-'"
            )));
        }
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let agent: Agent = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(https)
            .timeout_global(Some(timeout))
            .max_idle_connections(0)
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            agent,
            base: base_url.to_owned(),
            tailnet: tailnet.to_owned(),
            client,
            clock,
            cached: Mutex::new(None),
        })
    }

    /// The `Authorization` header value for the access token, fetching one when none is
    /// kept or the kept one is within [`TOKEN_REFRESH_MARGIN`] of its expiry.
    fn bearer(&self) -> Result<String, ApiFailure> {
        let now = self.clock.now();
        // The lock is not held across the request: `finish` takes it to drop a token.
        let kept = {
            let cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
            cached
                .as_ref()
                .filter(|kept| now.plus(TOKEN_REFRESH_MARGIN) < kept.expires)
                .map(|kept| format!("Bearer {}", kept.token.expose()))
        };
        if let Some(header) = kept {
            return Ok(header);
        }
        let url = format!("{}{API}/oauth/token", self.base);
        let grant = self.client.grant_body();
        let sent = self
            .agent
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .send(grant.expose());
        let body = self.finish(sent)?;
        let answer: TokenAnswer =
            serde_json::from_slice(&body).map_err(|_| ApiFailure::Malformed)?;
        if answer.access_token.is_empty() {
            return Err(ApiFailure::Malformed);
        }
        let header = format!("Bearer {}", answer.access_token);
        let expires = now.plus(Duration::from_secs(answer.expires_in));
        let token = Secret::new(answer.access_token);
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        *cached = Some(Cached { token, expires });
        Ok(header)
    }

    /// Forgets the kept access token, so the next call fetches one.
    fn drop_token(&self) {
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The body of a success, or the failure the status or the transport shows. A 401
    /// drops the kept token: it was revoked or is no longer good.
    fn finish(
        &self,
        sent: Result<ureq::http::Response<ureq::Body>, TransportError>,
    ) -> Result<Vec<u8>, ApiFailure> {
        let transport = |err: TransportError| ApiFailure::Transport(cut(&err.to_string()));
        let mut response = sent.map_err(transport)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(ANSWER_LIMIT)
            .read_to_vec()
            .map_err(transport)?;
        match status {
            200..=299 => Ok(body),
            401 => {
                self.drop_token();
                Err(ApiFailure::Rejected(status))
            }
            400..=499 => Err(ApiFailure::Rejected(status)),
            500..=599 => Err(ApiFailure::Unavailable(status)),
            _ => Err(ApiFailure::Unexpected(status)),
        }
    }

    fn get(&self, path: &str, bearer: &str) -> Result<Vec<u8>, ApiFailure> {
        let sent = self
            .agent
            .get(format!("{}{API}{path}", self.base))
            .header("authorization", bearer)
            .header("accept", "application/json")
            .call();
        self.finish(sent)
    }

    fn delete(&self, path: &str, bearer: &str) -> Result<Vec<u8>, ApiFailure> {
        let sent = self
            .agent
            .delete(format!("{}{API}{path}", self.base))
            .header("authorization", bearer)
            .call();
        self.finish(sent)
    }

    fn post_json(&self, path: &str, bearer: &str, body: &[u8]) -> Result<Vec<u8>, ApiFailure> {
        let sent = self
            .agent
            .post(format!("{}{API}{path}", self.base))
            .header("authorization", bearer)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .send(body);
        self.finish(sent)
    }

    /// Exchanges the OAuth client for an access token, or keeps the one it has. Creates
    /// nothing in the tailnet.
    ///
    /// # Errors
    ///
    /// [`NetworkError::Credentials`] when the exchange fails.
    pub fn check(&self) -> Result<(), NetworkError> {
        self.bearer().map(drop).map_err(NetworkError::Credentials)
    }

    /// Creates one auth key: single-use, ephemeral, pre-approved, tagged `tag`, valid for
    /// `valid_for_secs` seconds. The key is returned once and held as a [`Secret`].
    ///
    /// # Errors
    ///
    /// [`NetworkError::Credentials`] when there is no access token;
    /// [`NetworkError::KeyMint`] when the key cannot be created.
    pub fn mint_key(
        &self,
        tag: &str,
        description: &str,
        valid_for_secs: u64,
    ) -> Result<Secret, NetworkError> {
        let bearer = self.bearer().map_err(NetworkError::Credentials)?;
        let body = json!({
            "capabilities": {"devices": {"create": {
                "reusable": false,
                "ephemeral": true,
                "preauthorized": true,
                "tags": [tag],
            }}},
            "expirySeconds": valid_for_secs,
            "description": description,
        })
        .to_string();
        let path = format!("/tailnet/{}/keys", self.tailnet);
        let answer = self
            .post_json(&path, &bearer, body.as_bytes())
            .map_err(NetworkError::KeyMint)?;
        let key: KeyAnswer = serde_json::from_slice(&answer)
            .map_err(|_| NetworkError::KeyMint(ApiFailure::Malformed))?;
        match key.key.is_empty() {
            true => Err(NetworkError::KeyMint(ApiFailure::Malformed)),
            false => Ok(Secret::new(key.key)),
        }
    }

    /// Deletes every device of the tailnet whose hostname is `hostname`. A device that
    /// is already gone is not an error, so a repeat of a delete that happened succeeds.
    ///
    /// # Errors
    ///
    /// [`NetworkError::Credentials`] when there is no access token;
    /// [`NetworkError::DeviceDelete`] when the list or a delete fails.
    pub fn delete_devices(&self, hostname: &str) -> Result<(), NetworkError> {
        let failed = NetworkError::DeviceDelete;
        let bearer = self.bearer().map_err(NetworkError::Credentials)?;
        let listed = self
            .get(&format!("/tailnet/{}/devices", self.tailnet), &bearer)
            .map_err(failed)?;
        let list: Devices =
            serde_json::from_slice(&listed).map_err(|_| failed(ApiFailure::Malformed))?;
        for device in list.devices.iter().filter(|d| d.hostname == hostname) {
            if !path_segment(&device.id) {
                return Err(failed(ApiFailure::Malformed));
            }
            match self.delete(&format!("/device/{}", device.id), &bearer) {
                Ok(_) | Err(ApiFailure::Rejected(404)) => {}
                Err(other) => return Err(failed(other)),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts which texts stand as a path segment and which hosts are loopback,
    /// including bracketed IPv6 and a name that only starts like one.
    #[test]
    fn segments_and_loopback_hosts() {
        for ok in ["-", "a.b_c-1", "example.com"] {
            assert!(path_segment(ok), "{ok}");
        }
        for bad in ["", "a/b", "a b", "a?b", "é"] {
            assert!(!path_segment(bad), "{bad}");
        }
        for ok in ["127.0.0.1", "localhost", "[::1]", "127.9.9.9"] {
            assert!(loopback(ok), "{ok}");
        }
        for bad in [
            "10.0.0.1",
            "example.com",
            "127.0.0.1.example.com",
            "[2001:db8::1]",
        ] {
            assert!(!loopback(bad), "{bad}");
        }
        assert_eq!(cut(&"x".repeat(QUOTED + 5)).len(), QUOTED);
        assert_eq!(cut("short"), "short");
    }
}
